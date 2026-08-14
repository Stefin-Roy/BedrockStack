//! Intel HD Audio (ICH6/ICH9) controller driver.
//!
//! Polled driver for QEMU's `intel-hda` / `ich9-intel-hda` emulation.  The
//! controller moves verbs to the codec over the CORB/RIRB rings and plays
//! 16-bit signed stereo PCM at 48 kHz through the codec's output converter
//! (discovered generically by `super::codec`).  When the chosen codec also
//! exposes an input path (e.g. QEMU's `hda-duplex`), the same stream
//! machinery drives the input converter the other way for capture, so the
//! carriage rides in both directions.
//!
//! Playback runs as a single descriptor in a BDL (Buffer Descriptor List)
//! that the controller DMA's into the codec; capture is the mirror image —
//! the controller DMA's codec samples out of a BDL into the staging buffer.
//!
//! QEMU-specific facts this driver relies on (see hw/audio/intel-hda.c,
//! hw/audio/hda-codec.c):
//!   - GCAP = 0x4401 → ISS=4, OSS=4; the first output stream descriptor is
//!     at offset `0x80 + ISS*0x20` = 0x100, and QEMU decides stream direction
//!     by the register index (index >= 4 is an output), NOT by a bit in
//!     SDnCTL.  The base is derived from GCAP, not hardcoded.
//!   - RINTCNT must be non-zero before CORB DMA runs: `intel_hda_corb_run`
//!     bails while `rirb_count == rirb_cnt`, and RINTCNT resets to 0 on
//!     reset.  RIRBCTL.IRQ_EN must be set so the count gate latches
//!     `RIRBSTS.IRQ`; the polled driver clears that bit (write 1) on every
//!     verb to reset `rirb_count` and keep the ring flowing.
//!   - RIRBCTL must set DMA_EN (bit 1); without it `intel_hda_response`
//!     drops every response.
//!   - The controller reads the command at CORB index `(CORBRP + 1) & 0xff`,
//!     so commands must be written at `CORBWP + 1` (the Linux convention),
//!     not at the current CORBWP.
//!   - The controller derives the stream number from SDnCTL on every RUN-bit
//!     flip, so a stop must preserve the stream tag or the codec is told
//!     `stnr=0` and never stops.
//!   - A stream must be bound to the codec with `SET_CONV` (stream tag 1)
//!     and `SET_STREAM_FORMAT` before the controller's RUN bit is raised.

use alloc::boxed::Box;
use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;

use super::codec::{self, VerbSender};
use super::AudioDevice;
use crate::services::dma::DmaAllocator;
use crate::drivers::serial::SerialPort;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// DMA staging buffer capacity in bytes (one contiguous allocation).
const BUF_CAP: usize = 256 * 1024;

/// Output converter stream tag (matches SDnCTL bits 20..23).
const STREAM_TAG: u32 = 1;
/// Input converter stream tag, configured on the codec side only (capture is
/// not exposed yet, so no input stream descriptor is run).
const INPUT_TAG: u32 = 2;

// ── Controller register offsets ────────────────────────────────────
mod regs {
    pub const GCAP: u32 = 0x00;
    pub const GCTL: u32 = 0x08;
    pub const STATESTS: u32 = 0x0E;
    pub const CORBLBASE: u32 = 0x40;
    pub const CORBUBASE: u32 = 0x44;
    pub const CORBWP: u32 = 0x48;
    pub const CORBRP: u32 = 0x4A;
    pub const CORBCTL: u32 = 0x4C;
    pub const CORBSTS: u32 = 0x4D;
    pub const CORBSIZE: u32 = 0x4E;
    pub const RIRBLBASE: u32 = 0x50;
    pub const RIRBUBASE: u32 = 0x54;
    pub const RIRBWP: u32 = 0x58;
    pub const RINTCNT: u32 = 0x5A;
    pub const RIRBCTL: u32 = 0x5C;
    pub const RIRBSTS: u32 = 0x5D;
    pub const RIRBSIZE: u32 = 0x5E;
    // Stream descriptor sub-offsets (relative to the stream base).
    pub const SD_CTL: u32 = 0x00;
    // QEMU aliases SD_STS into the top byte of st->ctl (see intel-hda.c
    // regtab: .shift=24); byte 0x04 of that byte is BCIS, "descriptor
    // complete", latched whenever an IOC-flagged BDL entry is consumed.
    pub const SD_STS: u32 = 0x03;
    pub const SD_LPIB: u32 = 0x04;
    pub const SD_CBL: u32 = 0x08;
    pub const SD_LVI: u32 = 0x0C;
    pub const SD_FMT: u32 = 0x12; // SDnFMT is at 0x12 (0x10 is SDnFIFOS, R/O)
    pub const SD_BDPL: u32 = 0x18;
    pub const SD_BDPU: u32 = 0x1C;
}

// QEMU stream interrupts: INTCTL bit i enables stream i, bit 31 is the
// global enable.  INTSTS mirrors the pending sources; clearing SD_STS.BCIS
// deasserts the line (INTSTS recomputes automatically).
mod global_regs {
    pub const INTCTL: u32 = 0x20;
}

const GCTL_RSTCRST: u32 = 1;
/// CORBCTL bit 1 = CORBRUN (DMA run).  Bit 0 is CMEIE, *not* run.
const CORB_RUN: u8 = 1 << 1;
/// RIRBCTL value: bit 0 = RIRBIRQEN (response-count IRQ enable), bit 1 =
/// RIRBDMAEN.  There is no "RIRB run" bit; the DMA engine runs while
/// RIRBDMAEN is set.  IRQ_EN must be set so QEMU latches RIRBSTS.IRQ when
/// `rirb_count == RIRTCNT`; clearing that bit is what resets `rirb_count`.
const RIRB_CTL: u8 = (1 << 0) | (1 << 1);
/// RIRB status bits, all write-1-to-clear (Intel spec 3.3.30): RINTFL
/// (response-interrupt count reached), UNSOL (unsolicited response received),
/// RIRBOVERRUN (ring overrun).
const RIRB_INT_MASK: u8 = 0x07;
/// RINTCNT value: the maximum number of responses the controller accepts
/// before it gates CORB processing and latches RIRBSTS.IRQ.  The count must
/// be non-zero or QEMU's `intel_hda_corb_run` bails while
/// `rirb_count == rirb_cnt` (RINTCNT resets to 0).  A high value gives a
/// polled driver headroom; `codec_verb` clears RIRBSTS.IRQ on every command
/// to reset the count, so the gate never actually stalls the ring.
const RINTCNT_QUIET: u16 = 0xFF;

/// Per-verb time budget for the CORB/RIRB waits.  Real codecs can take tens
/// of ms to answer while powering up (they gate verbs on power state); QEMU
/// answers instantly, so it is unaffected.
const VERB_TIMEOUT_NS: u64 = 100_000_000;

const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;
/// Interrupt On Completion Enable — spec 3.3.35 bit 2: BCIS raises a
/// controller interrupt only while set (output's omission is a tolerated QEMU
/// deviation; the poll fallback covers it).  Input sets it so real hardware
/// gets completion interrupts too.
const SD_CTL_IOCE: u32 = 1 << 2;
const SD_CTL_STREAM_TAG: u32 = 1 << 20; // stream tag 1
/// Input stream tag — matches the codec's ADC `SET_CONV` binding (tag 2).
const SD_CTL_INPUT_STREAM_TAG: u32 = 2 << 20;

/// Controller stream descriptor format: 16-bit stereo, 48 kHz base rate.
///
/// Stream-format structure (spec 3.7.1, Table 53), shared by SDnFMT and the
/// codec `SET_STREAM_FORMAT` verb: TYPE=0 (PCM) · BASE=0 (48 kHz) ·
/// MULT=000 (÷1) · DIV=000 (÷1) · BITS=001 (16-bit) · CHAN=0001 (2 ch)
/// ⇒ 0x0011.  This MUST agree with the codec-side verb (`codec::STREAM_FMT_48K_STEREO_16`
/// is also 0x11); the previous 0x0A11 encoded 32 kHz (MULT=001, DIV=010) and
/// silently disagreed with the 48 kHz verb.
const SD_FMT_48K_STEREO_16: u16 = 0x0011;

const BDL_IOC: u32 = 0x01; // interrupt-on-complete flag

/// Stream-completion status bit (BCIS) within the SD_STS byte.
const SD_STS_BCIS: u8 = 0x04;

/// Number of BDL ring entries.  The 256 KiB DMA staging buffer is split into
/// equal slots of `BUF_CAP / RING_ENTRIES` bytes (32 KiB), giving ~1.3 s of
/// DMA headroom — far more than a disk read takes, so refills never starve.
const RING_ENTRIES: usize = 8;

// ── Interrupt state (lock-free, owned by the ISR) ──────────────────
//
// QEMU's intel-hda latches BCIS and raises the INTx/MSI line when an
// IOC-flagged BDL entry completes.  The ISR clears BCIS to deassert the
// line and counts the event; the playback/capture loops wait on the counter
// and refill the freed ring slot.  If the interrupt route is broken, the
// loops fall back to polling BCIS (QEMU latches it regardless of enablement).
// The output and input streams have separate descriptors and therefore
// separate BCIS bits; each is counted independently so a completion on one
// stream can never be mistaken for a completion on the other.

static HDA_MMIO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HDA_OUT_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Input stream descriptor base.  0 = sentinel for "capture not armed" (the
/// ISR must not touch a non-existent descriptor).
static HDA_IN_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static HDA_IRQ_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HDA_IN_IRQ_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Called from the device vector with interrupts disabled.  Acknowledges
/// every pending stream completion and counts each stream independently.
fn hda_irq_handler() {
    use core::sync::atomic::Ordering;
    let mmio = HDA_MMIO.load(Ordering::Relaxed);
    if mmio == 0 {
        return;
    }
    let ob = HDA_OUT_BASE.load(Ordering::Relaxed) as u64;
    let ib = HDA_IN_BASE.load(Ordering::Relaxed) as u64;
    unsafe {
        // Write-1-to-clear each BCIS: deasserts the INTx/MSI level.
        core::ptr::write_volatile((mmio + ob + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
        let _ = HDA_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
        if ib != 0 {
            core::ptr::write_volatile((mmio + ib + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
            let _ = HDA_IN_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct Inner {
    mmio: u64,
    corb_phys: u64,
    corb_virt: u64,
    rirb_phys: u64,
    rirb_virt: u64,
    bdl_phys: u64,
    bdl_virt: u64,
    buf_phys: u64,
    buf_virt: u64,
    /// Register offset of the first output stream.
    out_base: u32,
    /// Register offset of the first input stream (0x80; input descriptors
    /// 0..BSS live under the output block).  0 when capture is not armed.
    in_base: u32,
    /// RIRB write pointer seen so far (response ring consumption).
    last_wp: u16,
    /// BDL ring depth used by the streaming path.
    ring_entries: usize,
}

impl Inner {
    fn w8(&self, off: u32, v: u8) {
        unsafe { write_volatile((self.mmio + off as u64) as *mut u8, v) }
    }
    fn r8(&self, off: u32) -> u8 {
        unsafe { read_volatile((self.mmio + off as u64) as *const u8) }
    }
    fn r16(&self, off: u32) -> u16 {
        unsafe { read_volatile((self.mmio + off as u64) as *const u16) }
    }
    fn w16(&self, off: u32, v: u16) {
        unsafe { write_volatile((self.mmio + off as u64) as *mut u16, v) }
    }
    fn r32(&self, off: u32) -> u32 {
        unsafe { read_volatile((self.mmio + off as u64) as *const u32) }
    }
    fn w32(&self, off: u32, v: u32) {
        unsafe { write_volatile((self.mmio + off as u64) as *mut u32, v) }
    }

    /// Send one command verb over CORB and wait for its solicited RIRB
    /// response.  Commands are strictly serialised (one in flight at a time).
    fn codec_verb(&mut self, cmd: u32) -> Result<u32, &'static str> {
        // Clear the RIRB status bits (write-1-to-clear).  Clearing RINTFL is
        // what resets QEMU's `rirb_count` gate — QEMU stops processing CORB
        // once `rirb_count == RINTCNT`, and the count only resets when the
        // guest clears this bit.  One command is in flight per call, so this
        // keeps the count from ever reaching RINTCNT and stalling the ring.
        self.w8(regs::RIRBSTS, RIRB_INT_MASK);

        // Real codecs inject unsolicited responses (jack/HDMI events) that no
        // verb solicited, and a timed-out verb can leave a response behind.
        // Drain anything already in the ring so the solicited-response wait
        // below starts from a known write pointer.
        self.drain_rirb(true);

        // Wait for the CORB ring to drain.  The ring is empty when the read
        // and write pointers agree.  The waits here are time-based rather than
        // spin-count based: real codecs gate verb processing on their power
        // state, so a still-powering-up codec holds the ring busy far longer
        // than a spin budget tuned to QEMU's instant responses allows.  QEMU
        // answers immediately, so it is unaffected.
        let mmio = self.mmio;
        let drain_deadline = crate::services::universal_timer::now_ns() + VERB_TIMEOUT_NS;
        if !crate::services::universal_timer::wait_until_cond(drain_deadline, &|| {
            let rp = (unsafe { read_volatile((mmio + regs::CORBRP as u64) as *const u16) }) & 0xff;
            let wp = (unsafe { read_volatile((mmio + regs::CORBWP as u64) as *const u16) }) & 0xff;
            rp == wp
        }) {
            return Err("CORB ring not empty");
        }

        // The controller consumes the command at index `CORBRP + 1`, so the
        // command goes into the slot *after* CORBWP (Linux convention).
        let wp = self.r16(regs::CORBWP) & 0xff;
        let nxt = (wp + 1) & 0xff;
        unsafe { write_volatile((self.corb_virt + nxt as u64 * 4) as *mut u32, cmd) };
        self.w16(regs::CORBWP, nxt);

        // Wait for the controller to consume the command.
        let consumed_deadline = crate::services::universal_timer::now_ns() + VERB_TIMEOUT_NS;
        if !crate::services::universal_timer::wait_until_cond(consumed_deadline, &|| {
            let rp = (unsafe { read_volatile((mmio + regs::CORBRP as u64) as *const u16) }) & 0xff;
            rp == nxt
        }) {
            return Err("CORB command not consumed");
        }

        // Wait for the solicited response.  The RIRB is a FIFO: the
        // controller appends every response (solicited and unsolicited, for
        // any codec) and only advances RIRBWP.  Consume the batch from
        // `last_wp + 1 ..= RIRBWP`, skipping unsolicited entries (res_ex bit
        // 4) and responses addressed to another codec (res_ex low nibble), and
        // take the newest entry matching this command's codec address.  If a
        // batch turns out to contain no match, keep waiting within the same
        // deadline for the next batch.
        let rirb_virt = self.rirb_virt;
        let cad = (cmd >> 28) & 0xF;
        let mut last_wp = self.last_wp;
        let response_deadline = crate::services::universal_timer::now_ns() + VERB_TIMEOUT_NS;
        loop {
            let wp = (unsafe { read_volatile((mmio + regs::RIRBWP as u64) as *const u16) }) & 0xff;
            if wp == last_wp {
                let got = crate::services::universal_timer::wait_until_cond(response_deadline, &|| {
                    let w = (unsafe { read_volatile((mmio + regs::RIRBWP as u64) as *const u16) })
                        & 0xff;
                    w != last_wp
                });
                if !got {
                    // Diagnostic: dump the ring state so a persistent failure
                    // is visible in the serial log instead of just an error.
                    let wp_now = (unsafe {
                        read_volatile((mmio + regs::RIRBWP as u64) as *const u16)
                    }) & 0xff;
                    let res = unsafe { read_volatile((rirb_virt + wp_now as u64 * 8) as *const u32) };
                    let res_ex =
                        unsafe { read_volatile((rirb_virt + wp_now as u64 * 8 + 4) as *const u32) };
                    SerialPort::puts("[audio] hda: rirb timeout wp=0x");
                    SerialPort::put_hex(wp_now as u64);
                    SerialPort::puts(" last_wp=0x");
                    SerialPort::put_hex(last_wp as u64);
                    SerialPort::puts(" res=0x");
                    SerialPort::put_hex(res as u64);
                    SerialPort::puts(" res_ex=0x");
                    SerialPort::put_hex(res_ex as u64);
                    SerialPort::puts("\n");
                    self.last_wp = last_wp;
                    return Err("RIRB response timeout");
                }
                continue;
            }
            // FIFO-consume `last_wp + 1 ..= wp`.
            let mut idx = (last_wp + 1) & 0xff;
            let mut found: Option<u32> = None;
            while idx != ((wp + 1) & 0xff) {
                let res = unsafe { read_volatile((rirb_virt + idx as u64 * 8) as *const u32) };
                let res_ex = unsafe { read_volatile((rirb_virt + idx as u64 * 8 + 4) as *const u32) };
                if (res_ex & (1 << 4)) == 0 && (res_ex & 0xF) == cad {
                    found = Some(res);
                }
                idx = (idx + 1) & 0xff;
            }
            last_wp = wp;
            if let Some(r) = found {
                self.last_wp = last_wp;
                return Ok(r);
            }
        }
    }

    /// Consume every RIRB entry the controller has written so far, advancing
    /// `last_wp` to the current RIRBWP.  Real codecs inject unsolicited
    /// responses (jack/HDMI events) and a timed-out verb may leave an entry
    /// behind; draining keeps the ring in sync so the next verb's solicited
    /// response is found at a known index.  When `log_first`, prints one
    /// diagnostic line describing the first drained entry (proves stale
    /// entries were the cause of a truncated probe).  Returns the count
    /// drained.
    fn drain_rirb(&mut self, log_first: bool) -> u32 {
        let rirb_virt = self.rirb_virt;
        let wp = self.r16(regs::RIRBWP) & 0xff;
        let mut n = 0u32;
        let mut first = (0u32, 0u32);
        let mut idx = (self.last_wp + 1) & 0xff;
        while idx != ((wp + 1) & 0xff) {
            let res = unsafe { read_volatile((rirb_virt + idx as u64 * 8) as *const u32) };
            let res_ex = unsafe { read_volatile((rirb_virt + idx as u64 * 8 + 4) as *const u32) };
            if n == 0 {
                first = (res, res_ex);
            }
            n += 1;
            idx = (idx + 1) & 0xff;
        }
        if n > 0 && log_first {
            SerialPort::puts("[audio] hda: rirb drain n=");
            SerialPort::put_u64(n as u64);
            SerialPort::puts(" last_wp=0x");
            SerialPort::put_hex(self.last_wp as u64);
            SerialPort::puts(" wp=0x");
            SerialPort::put_hex(wp as u64);
            SerialPort::puts(" res=0x");
            SerialPort::put_hex(first.0 as u64);
            SerialPort::puts(" res_ex=0x");
            SerialPort::put_hex(first.1 as u64);
            SerialPort::puts("\n");
        }
        self.last_wp = wp;
        n
    }
}

impl VerbSender for Inner {
    fn verb(&mut self, cad: u32, nid: u32, v: u32, payload: u32) -> Result<u32, &'static str> {
        self.codec_verb(codec::verb(cad, nid, v, payload))
    }
}

pub struct HdaAudio {
    inner: Mutex<Inner>,
    /// Whether the input path is wired end-to-end (codec has an ADC and its
    /// input path came up).  Set once during init, read by `can_record`.
    cap_ready: core::sync::atomic::AtomicBool,
}

unsafe impl Send for HdaAudio {}
unsafe impl Sync for HdaAudio {}

impl AudioDevice for HdaAudio {
    fn name(&self) -> &str {
        "intel-hda"
    }

    fn play_pcm(&self, samples: &[i16]) -> Result<(), &'static str> {
        self.play(samples)
    }

    fn play_pcm_stream(
        &self,
        total_bytes: u32,
        entry_bytes: usize,
        next: &mut dyn FnMut() -> Option<alloc::vec::Vec<i16>>,
    ) -> Result<u64, &'static str> {
        self.play_stream(total_bytes, entry_bytes, next)
    }

    fn can_record(&self) -> bool {
        use core::sync::atomic::Ordering;
        self.cap_ready.load(Ordering::Acquire)
    }

    fn record_pcm(&self, dest: &mut [i16]) -> Result<(), &'static str> {
        self.record(dest)
    }

    fn record_pcm_stream(
        &self,
        total_bytes: u32,
        entry_bytes: usize,
        sink: &mut dyn FnMut(alloc::vec::Vec<i16>),
    ) -> Result<u64, &'static str> {
        self.record_stream(total_bytes, entry_bytes, sink)
    }
}

impl HdaAudio {
    fn play(&self, samples: &[i16]) -> Result<(), &'static str> {
        let i = self.inner.lock();
        let nbytes = samples.len() * 2;
        if nbytes == 0 {
            return Err("empty PCM");
        }
        if nbytes > BUF_CAP {
            return Err("PCM larger than DMA buffer");
        }

        // Stage the samples into the DMA staging buffer.
        unsafe {
            core::ptr::copy_nonoverlapping(
                samples.as_ptr() as *const u8,
                i.buf_virt as *mut u8,
                nbytes,
            );
        }

        // Build a single-entry BDL: { phys addr, 0, length, IOC }.
        let bdl = i.bdl_virt as *mut u64;
        unsafe {
            write_volatile(bdl, i.buf_phys);
            write_volatile(bdl.add(1), ((BDL_IOC as u64) << 32) | (nbytes as u64));
        }

        let ob = i.out_base;

        // Reset the stream (assert SRST, then deassert) while it is stopped.
        i.w32(ob + regs::SD_CTL, SD_CTL_SRST);
        core::hint::spin_loop();
        i.w32(ob + regs::SD_CTL, 0);

        // Program the stream descriptor.
        i.w32(ob + regs::SD_BDPL, i.bdl_phys as u32);
        i.w32(ob + regs::SD_BDPU, (i.bdl_phys >> 32) as u32);
        i.w16(ob + regs::SD_LVI, 0);
        i.w32(ob + regs::SD_CBL, nbytes as u32);
        i.w16(ob + regs::SD_FMT, SD_FMT_48K_STEREO_16);

        // Start DMA.
        i.w32(ob + regs::SD_CTL, SD_CTL_STREAM_TAG | SD_CTL_RUN);

        // Let the buffer play for its full duration, then wait for the ring
        // to drain before stopping.  LPIB (Link Position in Buffer) stays in
        // [0, nbytes) and wraps back to 0 once the whole cyclic buffer has
        // been consumed, so a wrap — LPIB moving backwards — is the drain
        // signal; `LPIB >= nbytes` can never be observed.
        let frames = samples.len() / CHANNELS;
        let ms = (frames as u64) * 1000 / SAMPLE_RATE as u64;
        crate::services::universal_timer::sleep_ms(ms);
        let started = i.r32(ob + regs::SD_LPIB);
        let deadline = crate::services::universal_timer::now_ns() + 500_000_000;
        crate::services::universal_timer::wait_until_cond(deadline, &|| {
            started == 0 || i.r32(ob + regs::SD_LPIB) < started
        });
        crate::services::universal_timer::sleep_ms(50);

        let lpib = i.r32(ob + regs::SD_LPIB);
        SerialPort::puts("[audio] hda: played ");
        SerialPort::put_u64(nbytes as u64);
        SerialPort::puts(" B (lpib=");
        SerialPort::put_u64(lpib as u64);
        SerialPort::puts(")\n");

        // Stop DMA but keep the stream tag.  QEMU derives the stream number
        // from SDnCTL on every RUN-bit flip (`intel_hda_set_st_ctl`), so
        // clearing the tag along with RUN would notify the codec with
        // stnr=0 and the converter (tag 1) would never stop — the codec's
        // output timer keeps pulling the wrapping BDL forever.
        i.w32(ob + regs::SD_CTL, SD_CTL_STREAM_TAG);
        Ok(())
    }

    /// Stream PCM through a continuously-running BDL ring.  The BDL geometry
    /// is programmed once (QEMU caches it in `st->bpl[]` at RUN start), so
    /// refills write PCM data into the ring slots, not the descriptors.
    fn play_stream(
        &self,
        total_bytes: u32,
        entry_bytes: usize,
        next: &mut dyn FnMut() -> Option<alloc::vec::Vec<i16>>,
    ) -> Result<u64, &'static str> {
        let i = self.inner.lock();
        let ob = i.out_base;
        let n = i.ring_entries;
        let eb = entry_bytes;
        let ring_cap = BUF_CAP / n;
        if eb == 0 || eb > ring_cap || total_bytes == 0 {
            return Err("bad stream params");
        }

        // Number of `eb`-sized entries the payload occupies.  Every ring slot
        // is `eb` bytes long; a short final chunk is zero-padded so the
        // fixed-geometry BDL stays consistent across ring wraps.
        let needed = (total_bytes as u64).div_ceil(eb as u64) as usize;
        // QEMU stops transferring when LPIB reaches CBL, so CBL must cover
        // the padded total or the last entry never completes.
        let cbl = (needed as u64).saturating_mul(eb as u64) as u32;

        let bdl = i.bdl_virt as *mut u64;
        let buf_virt = i.buf_virt as *mut u8;
        let buf_phys = i.buf_phys;

        // Program the ring descriptors: used slots are all `eb`-sized and
        // carry IOC; unused slots (payload shorter than the ring) are
        // zero-length with no IOC and are skipped without a completion.
        for k in 0..n {
            let used = k < needed;
            let len = if used { eb } else { 0 };
            let flags = if used { BDL_IOC } else { 0 };
            unsafe {
                write_volatile(bdl.add(k * 2), buf_phys + (k as u64) * eb as u64);
                write_volatile(bdl.add(k * 2 + 1), ((flags as u64) << 32) | (len as u64));
            }
        }

        // Stage chunk `chunk_idx` into its ring slot (`chunk_idx % n`),
        // zero-padding a short final chunk.  Returns the real bytes staged.
        let stage = |chunk_idx: usize, chunk: &[i16]| -> Result<u64, &'static str> {
            let bytes = chunk.len() * 2;
            if bytes == 0 || bytes > eb {
                return Err("bad chunk size");
            }
            unsafe {
                let dst = buf_virt.add((chunk_idx % n) * eb);
                core::ptr::write_bytes(dst, 0, eb);
                core::ptr::copy_nonoverlapping(chunk.as_ptr() as *const u8, dst, bytes);
            }
            Ok(bytes as u64)
        };

        // Pre-fill the ring with the first `min(needed, n)` chunks.
        let prefilled = needed.min(n);
        let mut fed_real = 0u64;
        for k in 0..prefilled {
            let chunk = next().ok_or("stream ended early")?;
            fed_real += stage(k, &chunk)?;
        }

        // Clear any stale completion left by a previous run, then reset the
        // stream descriptor and program the ring.
        i.w8(ob + regs::SD_STS, SD_STS_BCIS);
        i.w32(ob + regs::SD_CTL, SD_CTL_SRST);
        core::hint::spin_loop();
        i.w32(ob + regs::SD_CTL, 0);
        i.w32(ob + regs::SD_BDPL, i.bdl_phys as u32);
        i.w32(ob + regs::SD_BDPU, (i.bdl_phys >> 32) as u32);
        i.w16(ob + regs::SD_LVI, (n - 1) as u16);
        i.w32(ob + regs::SD_CBL, cbl);
        i.w16(ob + regs::SD_FMT, SD_FMT_48K_STEREO_16);

        // Start DMA.
        i.w32(ob + regs::SD_CTL, SD_CTL_STREAM_TAG | SD_CTL_RUN);

        // Refill the ring as entries complete.  The engine keeps cycling the
        // descriptors, so each completion frees one slot (`completed - 1` mod
        // n); that slot is next played as chunk `completed - 1 + n`.
        let mut irq_seen = HDA_IRQ_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let mut completed: u64 = 0;
        while completed < needed as u64 {
            let seen = irq_seen;
            let deadline = crate::services::universal_timer::now_ns() + 1_000_000_000;
            let got = crate::services::universal_timer::wait_until_cond(deadline, &|| {
                HDA_IRQ_COUNT.load(core::sync::atomic::Ordering::Relaxed) != seen
                    || i.r8(ob + regs::SD_STS) & SD_STS_BCIS != 0
            });
            if !got {
                // Entries complete every ~170 ms; a full second without one
                // means the DMA stalled — don't fabricate a completion.
                i.w32(ob + regs::SD_CTL, SD_CTL_STREAM_TAG);
                return Err("playback stalled");
            }
            // Consume the completion exactly once: the ISR already cleared
            // BCIS (counter moved) or we clear it here (poll fallback).
            if HDA_IRQ_COUNT.load(core::sync::atomic::Ordering::Relaxed) != seen {
                irq_seen = HDA_IRQ_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            } else {
                i.w8(ob + regs::SD_STS, SD_STS_BCIS);
            }
            completed += 1;

            let next_chunk = (completed - 1) + n as u64;
            if next_chunk < needed as u64 {
                let chunk = next().ok_or("stream ended early")?;
                stage((next_chunk % n as u64) as usize, &chunk)?;
            }
        }

        // Let the codec's FIFO drain the final bytes, then stop the stream
        // (keeping the tag, as in `play`).
        crate::services::universal_timer::sleep_ms(20);
        i.w32(ob + regs::SD_CTL, SD_CTL_STREAM_TAG);
        Ok(total_bytes as u64)
    }

    /// Record a whole buffer: `dest.len()` i16 samples of interleaved stereo
    /// 48 kHz audio, captured through a two-entry BDL into the staging buffer
    /// and copied out once the DMA has delivered the full length.  Blocking;
    /// the mirror image of `play`.
    fn record(&self, dest: &mut [i16]) -> Result<(), &'static str> {
        use core::sync::atomic::Ordering;
        if !self.cap_ready.load(Ordering::Acquire) {
            return Err("capture not supported");
        }
        let i = self.inner.lock();
        let nbytes = dest.len() * 2;
        if nbytes == 0 {
            return Err("empty PCM");
        }
        if nbytes > BUF_CAP {
            return Err("PCM larger than DMA buffer");
        }
        // BDL lengths must be an integer number of 32-bit words (3.6.3).
        if nbytes % 4 != 0 {
            return Err("PCM length not word-aligned");
        }
        let ib = i.in_base;

        // Two-entry BDL: the whole buffer split at a 128-byte boundary, both
        // IOC.  The spec requires LVI >= 1 — at least two valid descriptors
        // before DMA can begin (3.3.39) — and every buffer start 128-byte
        // aligned (3.6.3); splitting at `buf_phys + split` keeps both aligned.
        let split = if nbytes >= 256 {
            (nbytes / 2) / 128 * 128
        } else {
            nbytes.min(128)
        };
        let e0 = split;
        let e1 = nbytes - split;
        let bdl = i.bdl_virt as *mut u64;
        unsafe {
            write_volatile(bdl, i.buf_phys);
            write_volatile(bdl.add(1), ((BDL_IOC as u64) << 32) | (e0 as u64));
            write_volatile(bdl.add(2), i.buf_phys + split as u64);
            write_volatile(bdl.add(3), ((BDL_IOC as u64) << 32) | (e1 as u64));
        }

        // Reset the input stream (assert SRST, then deassert) while stopped.
        // RUN must be clear before SRST is asserted (3.3.35, bit 0), which it
        // is: every prior stop leaves RUN=0 and the tag in SDnCTL.
        i.w32(ib + regs::SD_CTL, SD_CTL_SRST);
        core::hint::spin_loop();
        i.w32(ib + regs::SD_CTL, 0);

        // Program the stream descriptor (CBL/LVI may only be written after a
        // reset and with RUN=0, per 3.3.38/3.3.39).
        i.w32(ib + regs::SD_BDPL, i.bdl_phys as u32);
        i.w32(ib + regs::SD_BDPU, (i.bdl_phys >> 32) as u32);
        i.w16(ib + regs::SD_LVI, 1);
        i.w32(ib + regs::SD_CBL, nbytes as u32);
        i.w16(ib + regs::SD_FMT, SD_FMT_48K_STEREO_16);

        // Start DMA.  The input tag (2) matches the codec's ADC `SET_CONV`;
        // as with output, a stop must preserve it in SDnCTL.
        let mut irq_seen = HDA_IN_IRQ_COUNT.load(Ordering::Relaxed);
        i.w8(ib + regs::SD_STS, SD_STS_BCIS);
        i.w32(
            ib + regs::SD_CTL,
            SD_CTL_INPUT_STREAM_TAG | SD_CTL_RUN | SD_CTL_IOCE,
        );

        // Let the capture run for the full duration, then wait for both BDL
        // entries to complete (IRQ count delta = 2, or two BCIS poll-clear
        // cycles when no IRQ is wired) before draining the FIFO.  If no audio
        // source is connected QEMU still delivers silence, so the DMA
        // advances and the buffer fills; a still-missing completion is
        // tolerated, mirroring `play`'s tolerance of a missing wrap.
        let frames = dest.len() / CHANNELS;
        let ms = (frames as u64) * 1000 / SAMPLE_RATE as u64;
        crate::services::universal_timer::sleep_ms(ms);
        let mut remaining = 2u64;
        let deadline = crate::services::universal_timer::now_ns() + 500_000_000;
        while remaining > 0 {
            if !crate::services::universal_timer::wait_until_cond(deadline, &|| {
                HDA_IN_IRQ_COUNT.load(Ordering::Relaxed) != irq_seen
                    || i.r8(ib + regs::SD_STS) & SD_STS_BCIS != 0
            }) {
                break;
            }
            if HDA_IN_IRQ_COUNT.load(Ordering::Relaxed) != irq_seen {
                let cur = HDA_IN_IRQ_COUNT.load(Ordering::Relaxed);
                let delta = cur - irq_seen;
                irq_seen = cur;
                remaining = remaining.saturating_sub(delta);
            } else {
                i.w8(ib + regs::SD_STS, SD_STS_BCIS);
                remaining -= 1;
            }
        }
        crate::services::universal_timer::sleep_ms(50);

        // Copy the captured samples out of the live DMA buffer.
        unsafe {
            core::ptr::copy_nonoverlapping(
                i.buf_virt as *const u8,
                dest.as_mut_ptr() as *mut u8,
                nbytes,
            );
        }

        let lpib = i.r32(ib + regs::SD_LPIB);
        SerialPort::puts("[audio] hda: captured ");
        SerialPort::put_u64(nbytes as u64);
        SerialPort::puts(" B (lpib=");
        SerialPort::put_u64(lpib as u64);
        SerialPort::puts(")\n");

        // Stop DMA but keep the input stream tag in SDnCTL.
        i.w32(ib + regs::SD_CTL, SD_CTL_INPUT_STREAM_TAG);
        Ok(())
    }

    /// Record PCM through a continuously-running BDL ring, mirroring
    /// `play_stream`.  The ring geometry is programmed once; each completion
    /// frees one slot, which is copied out into an owned chunk (the ring is
    /// live DMA memory being overwritten by the controller) and handed to
    /// `sink`.  The final chunk is trimmed to the exact requested size.
    fn record_stream(
        &self,
        total_bytes: u32,
        entry_bytes: usize,
        sink: &mut dyn FnMut(alloc::vec::Vec<i16>),
    ) -> Result<u64, &'static str> {
        use core::sync::atomic::Ordering;
        if !self.cap_ready.load(Ordering::Acquire) {
            return Err("capture not supported");
        }
        let i = self.inner.lock();
        let ib = i.in_base;
        let n = i.ring_entries;
        let eb = entry_bytes;
        let ring_cap = BUF_CAP / n;
        if eb == 0 || eb > ring_cap || total_bytes == 0 {
            return Err("bad stream params");
        }
        // Every ring slot must keep the 128-byte buffer alignment of 3.6.3.
        if eb % 128 != 0 {
            return Err("entry size not 128-byte aligned");
        }

        // Number of `eb`-sized entries the payload occupies.  Ring slots are
        // all `eb` bytes long and CBL is the padded total, so the fixed
        // geometry holds across ring wraps; the final slot's trailing capture
        // is discarded by the trim below.
        let needed = (total_bytes as u64).div_ceil(eb as u64) as usize;
        let cbl = (needed as u64).saturating_mul(eb as u64) as u32;

        let bdl = i.bdl_virt as *mut u64;
        for k in 0..n {
            let used = k < needed;
            let len = if used { eb } else { 0 };
            let flags = if used { BDL_IOC } else { 0 };
            unsafe {
                write_volatile(bdl.add(k * 2), i.buf_phys + (k as u64) * eb as u64);
                write_volatile(bdl.add(k * 2 + 1), ((flags as u64) << 32) | (len as u64));
            }
        }

        // Clear any stale completion, reset the stream, program the ring.
        i.w8(ib + regs::SD_STS, SD_STS_BCIS);
        i.w32(ib + regs::SD_CTL, SD_CTL_SRST);
        core::hint::spin_loop();
        i.w32(ib + regs::SD_CTL, 0);
        i.w32(ib + regs::SD_BDPL, i.bdl_phys as u32);
        i.w32(ib + regs::SD_BDPU, (i.bdl_phys >> 32) as u32);
        i.w16(ib + regs::SD_LVI, (n - 1) as u16);
        i.w32(ib + regs::SD_CBL, cbl);
        i.w16(ib + regs::SD_FMT, SD_FMT_48K_STEREO_16);

        // Start DMA.
        let mut irq_seen = HDA_IN_IRQ_COUNT.load(Ordering::Relaxed);
        i.w32(
            ib + regs::SD_CTL,
            SD_CTL_INPUT_STREAM_TAG | SD_CTL_RUN | SD_CTL_IOCE,
        );

        // Drain completions; completion `completed - 1` filled slot
        // `(completed - 1) mod n`, which the controller will not revisit
        // until the ring has cycled n entries later.
        let mut completed: u64 = 0;
        let mut recorded: u64 = 0;
        while completed < needed as u64 {
            let seen = irq_seen;
            let deadline = crate::services::universal_timer::now_ns() + 1_000_000_000;
            let got = crate::services::universal_timer::wait_until_cond(deadline, &|| {
                HDA_IN_IRQ_COUNT.load(Ordering::Relaxed) != seen
                    || i.r8(ib + regs::SD_STS) & SD_STS_BCIS != 0
            });
            if !got {
                i.w32(ib + regs::SD_CTL, SD_CTL_INPUT_STREAM_TAG);
                return Err("capture stalled");
            }
            if HDA_IN_IRQ_COUNT.load(Ordering::Relaxed) != seen {
                irq_seen = HDA_IN_IRQ_COUNT.load(Ordering::Relaxed);
            } else {
                i.w8(ib + regs::SD_STS, SD_STS_BCIS);
            }
            completed += 1;

            let slot = ((completed - 1) % n as u64) as usize;
            let bytes = if (completed as usize) == needed {
                total_bytes as usize - (needed - 1) * eb
            } else {
                eb
            };
            let n_i16 = bytes / 2;
            let mut chunk = alloc::vec![0i16; n_i16];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (i.buf_virt + slot as u64 * eb as u64) as *const u8,
                    chunk.as_mut_ptr() as *mut u8,
                    bytes,
                );
            }
            sink(chunk);
            recorded += bytes as u64;
        }

        // Allow the codec FIFO to settle, then stop keeping the input tag.
        crate::services::universal_timer::sleep_ms(20);
        i.w32(ib + regs::SD_CTL, SD_CTL_INPUT_STREAM_TAG);
        Ok(recorded)
    }
}

/// Bring up the controller at `dev` and return it as a leakable device.
pub fn init(dev: &crate::pci::PciDevice) -> Result<&'static dyn AudioDevice, &'static str> {
    // Real hardware can hand the HDA function over with memory decode and
    // bus master disabled in the PCI Command register; BAR0 then reads as
    // all-ones and no codec is ever seen.  Enable them up front (QEMU is a
    // no-op — OVMF already did it).  Mirrors Linux pci_enable_device().
    crate::pci::enable_device(dev);

    let base = match crate::pci::bar::bar(dev, 0) {
        crate::pci::bar::Bar::Memory { addr, .. } => addr,
        _ => return Err("HDA BAR0 is not memory-mapped"),
    };

    let dma: &dyn DmaAllocator = crate::services::kernel_services().dma;

    // BAR0 covers 0x4000 on QEMU (a 0x2000 register window mirrored above).
    let mmio = dma.map_mmio(base, 0x4000)?;
    let corb = dma.alloc_page().ok_or("OOM CORB")?;
    let rirb = dma.alloc_page().ok_or("OOM RIRB")?;
    let bdl = dma.alloc_page().ok_or("OOM BDL")?;
    let buf = dma.alloc_contiguous(BUF_CAP / 4096).ok_or("OOM PCM buffer")?;

    let audio = Box::new(HdaAudio {
        inner: Mutex::new(Inner {
            mmio,
            corb_phys: corb.phys,
            corb_virt: corb.virt,
            rirb_phys: rirb.phys,
            rirb_virt: rirb.virt,
            bdl_phys: bdl.phys,
            bdl_virt: bdl.virt,
            buf_phys: buf.phys,
            buf_virt: buf.virt,
            out_base: 0,
            in_base: 0,
            last_wp: 0,
            ring_entries: RING_ENTRIES,
        }),
        cap_ready: core::sync::atomic::AtomicBool::new(false),
    });
    let audio: &'static HdaAudio = Box::leak(audio);

    {
        let mut i = audio.inner.lock();

        // Controller reset: CRST (GCTL bit 0) is active-high for operational
        // mode — 0 = held in reset, 1 = operational (Intel spec 3.3.7).
        // Assert reset (QEMU cold-resets on a write that leaves CRST=0), wait
        // for the controller to report CRST=0, then take it out of reset and
        // wait until CRST reads back 1.  The poll is required on real
        // hardware; QEMU transitions instantly so the loops exit immediately.
        SerialPort::puts("[audio] hda: controller reset\n");
        i.w32(regs::GCTL, 0);
        for _ in 0..1000 {
            if i.r32(regs::GCTL) & GCTL_RSTCRST == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        i.w32(regs::GCTL, GCTL_RSTCRST);
        for _ in 0..1000 {
            if i.r32(regs::GCTL) & GCTL_RSTCRST != 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Codecs reset asynchronously after the controller comes out of CRST;
        // a real codec can take a few ms to report present on the link.  Poll
        // STATESTS for a codec to appear (bounded ~100 ms) instead of reading
        // it once immediately after reset.  QEMU sets the bits instantly, so
        // this returns immediately there.
        let c_mmio = i.mmio;
        let present_deadline = crate::services::universal_timer::now_ns() + 100_000_000;
        crate::services::universal_timer::wait_until_cond(present_deadline, &|| {
            (unsafe { read_volatile((c_mmio + regs::STATESTS as u64) as *const u16) }) != 0
        });
        let sts = i.r16(regs::STATESTS);

        // Read capabilities and compute the first output stream base.
        // GCAP bits: [15:12] OSS, [11:8] ISS, [7:4] BSS, [3:0] NSDO.  Input
        // streams occupy descriptors 0..ISS-1, so the first output stream is
        // at offset 0x80 + ISS*0x20.  QEMU's ich9/intel-hda reports
        // GCAP = 0x4401 → ISS=4 → out_base = 0x100.
        let gcap = i.r16(regs::GCAP);
        let iss = (gcap >> 8) & 0x0f;
        let oss = (gcap >> 12) & 0x0f;
        let out_base = 0x80 + (iss as u32) * 0x20;
        i.out_base = out_base;
        // Input streams live at descriptor 0 (register index < 4 is an input
        // stream; QEMU decides direction by the register index, not a
        // descriptor bit).  Armed only when a codec with an ADC is selected.
        let in_base = 0x80u32;
        SerialPort::puts("[audio] hda: iss=");
        SerialPort::put_u64(iss as u64);
        SerialPort::puts(" oss=");
        SerialPort::put_u64(oss as u64);
        SerialPort::puts(" out_base=0x");
        SerialPort::put_hex(out_base as u64);
        SerialPort::puts("\n");

        // CORB: base pointers, program the ring size (0x02 = 256 entries, the
        // encoding Linux uses and the size our `& 0xff` pointer masking and
        // QEMU's internal ring assume), reset pointers, then run.
        i.w32(regs::CORBLBASE, i.corb_phys as u32);
        i.w32(regs::CORBUBASE, (i.corb_phys >> 32) as u32);
        i.w8(regs::CORBSIZE, 0x02);
        i.w16(regs::CORBRP, 0x8000);
        i.w16(regs::CORBRP, 0);
        i.w16(regs::CORBWP, 0);
        i.w8(regs::CORBSTS, 0);
        i.w8(regs::CORBCTL, CORB_RUN);

        // RIRB: base pointers, ring size (256 entries), reset pointers, then
        // enable DMA.  RINTCNT must be non-zero or the controller never drains
        // the CORB; RIRBCTL must set DMA_EN or responses are dropped (and
        // IRQ_EN so the response-count gate can be cleared — see
        // `codec_verb`).
        i.w32(regs::RIRBLBASE, i.rirb_phys as u32);
        i.w32(regs::RIRBUBASE, (i.rirb_phys >> 32) as u32);
        i.w8(regs::RIRBSIZE, 0x02);
        i.w16(regs::RIRBWP, 0x8000);
        i.w16(regs::RIRBWP, 0);
        i.w8(regs::RIRBSTS, 0);
        i.w16(regs::RINTCNT, RINTCNT_QUIET);
        i.w8(regs::RIRBCTL, RIRB_CTL);
        i.last_wp = 0;

        // Real codecs finish powering up a few ms after the link reset and may
        // answer their first verbs with the error-default (0) or stall the
        // ring while still waking.  Ping each present codec until its vendor
        // ID reads back stable and non-zero, bounded at ~200 ms, before the
        // full probe.  QEMU answers the first read instantly, so this is a
        // no-op there.
        for cad in 0..16u32 {
            if sts & (1 << cad) == 0 {
                continue;
            }
            let cmd = codec::verb(cad, 0, codec::VERB_GET_PARAM, codec::PARAM_VENDOR_ID);
            let ready_deadline = crate::services::universal_timer::now_ns() + 200_000_000;
            loop {
                match i.codec_verb(cmd) {
                    Ok(v) if v != 0 && v != 0xFFFF_FFFF => break,
                    _ => {
                        if crate::services::universal_timer::now_ns() >= ready_deadline {
                            SerialPort::puts("[audio] hda: codec ");
                            SerialPort::put_u64(cad as u64);
                            SerialPort::puts(" not ready, probing anyway\n");
                            break;
                        }
                    }
                }
            }
        }

        // Discover attached codecs via STATESTS (bit i = codec i present),
        // probe each, and keep a usable one.  Prefer a codec with an analog
        // output (line-out/speaker/headphone) over a digital-only one such as
        // the Intel HDMI/DP function, which has no path to the speakers;
        // digital is only a fallback.  Among analog-output codecs, one that
        // *also* has an ADC wins: with both `hda-output` (cad 0, no ADC) and
        // `hda-duplex` (cad 1, DAC+ADC) attached, the first probed analog
        // codec is output-only, so picking it would silently lose capture.
        SerialPort::puts("[audio] hda: states=0x");
        SerialPort::put_hex(sts as u64);
        SerialPort::puts("\n");

        let mut codec: Option<codec::Codec> = None;
        // An analog-output codec that can also capture (two-way preference).
        let mut duplex: Option<codec::Codec> = None;
        let mut digital: Option<codec::Codec> = None;
        for cad in 0..16u32 {
            if sts & (1 << cad) == 0 {
                continue;
            }
            match codec::probe(&mut *i, cad) {
                Ok(c) => {
                    SerialPort::puts("[audio] hda: codec ");
                    SerialPort::put_u64(cad as u64);
                    SerialPort::puts(" vendor=0x");
                    SerialPort::put_hex(c.vendor as u64);
                    SerialPort::puts(" subsys=0x");
                    SerialPort::put_hex(c.subsystem as u64);
                    SerialPort::puts(" rev=0x");
                    SerialPort::put_hex(c.rev as u64);
                    SerialPort::puts(" fg=");
                    SerialPort::put_u64(c.fg as u64);
                    SerialPort::puts(" wnc=0x");
                    SerialPort::put_hex(c.wnc as u64);
                    SerialPort::puts(" widgets=");
                    SerialPort::put_u64(c.widgets.len() as u64);
                    SerialPort::puts("\n");
                    if codec::is_realtek_alc256(c.vendor) {
                        // ALC256 is the analog codec that reaches the speakers.
                        // Prefer it over any digital (HDMI) function even when
                        // the generic walk failed to enumerate it; it is
                        // brought up via the hardcoded path below.
                        if codec.is_none() {
                            codec = Some(c);
                        }
                    } else if let Some(dac) = c.dac {
                        SerialPort::puts("[audio] hda:   output dac=");
                        SerialPort::put_u64(dac as u64);
                        SerialPort::puts(" pin=");
                        SerialPort::put_u64(c.out_pin.unwrap_or(0) as u64);
                        SerialPort::puts(" fmt=0x");
                        SerialPort::put_hex(c.fmt as u64);
                        if let Some(adc) = c.adc {
                            SerialPort::puts(" | input adc=");
                            SerialPort::put_u64(adc as u64);
                            SerialPort::puts(" pin=");
                            SerialPort::put_u64(c.in_pin.unwrap_or(0) as u64);
                        }
                        SerialPort::puts("\n");
                        if c.out_is_analog() {
                            if c.adc.is_some() {
                                if duplex.is_none() {
                                    duplex = Some(c);
                                }
                            } else if codec.is_none() {
                                codec = Some(c);
                            }
                        } else if digital.is_none() {
                            digital = Some(c);
                        }
                    }
                }
                Err(e) => {
                    SerialPort::puts("[audio] hda: codec ");
                    SerialPort::put_u64(cad as u64);
                    SerialPort::puts(" probe: ");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                }
            }
        }
        let codec = duplex.or(codec).or(digital).ok_or("no usable codec")?;

        // Bring up the output path (power, amps, pin, stream binding), then
        // the input path when the codec has an ADC.  An ALC256 whose widget
        // walk was truncated gets its hardcoded analog path instead of the
        // generic one (and, lacking a hardcoded input binding, stays
        // playback-only).  Capture is armed only when setup succeeds.
        if codec::is_realtek_alc256(codec.vendor) {
            SerialPort::puts("[audio] hda: alc256 hardcoded analog path\n");
            codec::setup_alc256_output(&mut *i, &codec, STREAM_TAG)?;
        } else {
            codec::setup_output(&mut *i, &codec, STREAM_TAG)?;
        }
        let mut cap_ok = codec.adc.is_some();
        if cap_ok {
            match codec::setup_input(&mut *i, &codec, INPUT_TAG) {
                Ok(()) => SerialPort::puts("[audio] hda: input path ready\n"),
                Err(e) => {
                    SerialPort::puts("[audio] hda: input path failed: ");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                    cap_ok = false;
                }
            }
        }
        audio.cap_ready.store(cap_ok, core::sync::atomic::Ordering::Release);
        if cap_ok {
            i.in_base = in_base;
        }

        // Publish the registers the completion ISR needs (lock-free), then
        // enable stream-completion interrupts.  Best-effort: playback falls
        // back to polling BCIS if the route can't be established.
        HDA_MMIO.store(mmio, core::sync::atomic::Ordering::Release);
        HDA_OUT_BASE.store(out_base, core::sync::atomic::Ordering::Release);
        if cap_ok {
            HDA_IN_BASE.store(in_base, core::sync::atomic::Ordering::Release);
        }
        #[cfg(target_arch = "x86_64")]
        setup_stream_interrupt(dev, out_base, in_base, cap_ok);
    }

    Ok(audio)
}

/// Enable stream-completion interrupts for the output (and, when capture is
/// armed, input) streams: MSI when the controller exposes a capability,
/// legacy INTx otherwise.  QEMU's intel-hda advertises MSI by default, so the
/// MSI path is the normal one.
#[cfg(target_arch = "x86_64")]
fn setup_stream_interrupt(dev: &crate::pci::PciDevice, out_base: u32, in_base: u32, cap_ok: bool) {
    use crate::arch::x86_64::idt;
    use crate::pci::caps;
    use crate::drivers::serial::SerialPort;

    let Some(vector) = idt::register_device_handler(hda_irq_handler) else {
        SerialPort::puts("[audio] hda: no device vector free, polling BCIS\n");
        return;
    };

    // Stream index of the output descriptor, for INTCTL bit selection.
    let stream_index = (out_base - 0x80) / 0x20;

    let caps_list = caps::all(dev);
    if let Some(msi) = caps_list.iter().find(|c| c.id == caps::CAP_MSI) {
        let bsp_apic_id = unsafe {
            let lapic = crate::platform::x86_64_pc::apic::lapic_base();
            core::ptr::read_volatile((lapic as *const u32).add(0x20 / 4)) >> 24
        } as u8;
        crate::pci::msi::enable(dev, msi, vector, bsp_apic_id);
        SerialPort::puts("[audio] hda: MSI enabled\n");
    } else if dev.interrupt_line != 0 {
        if crate::platform::x86_64_pc::ioapic::enable_irq(
            dev.interrupt_line as u32,
            crate::acpi::Polarity::ActiveLow,
            crate::acpi::TriggerMode::Level,
        )
        .is_none()
        {
            idt::unregister_device_handler(vector);
            SerialPort::puts("[audio] hda: INTx route failed, polling BCIS\n");
            return;
        }
        SerialPort::puts("[audio] hda: INTx enabled\n");
    } else {
        idt::unregister_device_handler(vector);
        SerialPort::puts("[audio] hda: no interrupt source, polling BCIS\n");
        return;
    }

    // Gate completion interrupts for the output and (when armed) input
    // streams, plus the global enable.
    let mut intctl = (1 << stream_index) | (1 << 31);
    if cap_ok {
        let in_stream_index = (in_base - 0x80) / 0x20;
        intctl |= 1 << in_stream_index;
    }
    unsafe {
        let mmio = HDA_MMIO.load(core::sync::atomic::Ordering::Relaxed);
        core::ptr::write_volatile((mmio + global_regs::INTCTL as u64) as *mut u32, intctl);
    }
}
