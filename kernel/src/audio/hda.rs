//! Intel HD Audio (ICH6/ICH9) controller driver.
//!
//! Polled/IRQ driver for QEMU's `intel-hda` / `ich9-intel-hda` emulation.  The
//! controller moves verbs to the codec over the CORB/RIRB rings and plays
//! 16-bit signed stereo PCM at 48 kHz through the codec's output converter
//! (discovered generically by `super::codec`).  When the chosen codec also
//! exposes an input path (e.g. QEMU's `hda-duplex`), the same ring machinery
//! drives the input converter the other way for capture.
//!
//! ## Feeding ring model
//!
//! Each direction owns one fixed-geometry cyclic BDL ring that is programmed
//! **once at init** and left running for the kernel's lifetime.  The DMA never
//! stops, so back-to-back calls chain gaplessly and there is no per-call stream
//! reset or descriptor reprogramming.
//!
//! Ownership is tracked with two **byte** cursors per direction (the DMA works
//! in 4096-byte BDL slots, but callers may stage or read any byte count, so a
//! partial slot simply carries over into the next call — no data is lost and no
//! silence is inserted at slot boundaries):
//!
//! - **Playback**: `OUT_PRODUCED` (bytes staged by callers) and
//!   `OUT_COMPLETED` (bytes the DMA has finished playing, advanced by a full
//!   slot in the completion ISR).  A caller stages bytes at
//!   `OUT_PRODUCED % ring_bytes` whenever the linear distance
//!   `OUT_PRODUCED - OUT_COMPLETED` is less than the ring size — i.e. whenever
//!   it is not running a full ring ahead of the play head (which would alias
//!   the write back onto the DMA's in-progress slot).  When the ring is full
//!   the caller parks cooperatively until a completion frees space.  Each
//!   consumed output slot is zeroed by the ISR, so a stalled producer plays
//!   **silence**, never a stale tail from an earlier lap.
//!
//! - **Capture**: `IN_CAPTURED` (bytes filled by the input DMA, advanced by a
//!   full slot in the ISR) and `IN_CONSUMED` (bytes read out by callers).  A
//!   caller reads `IN_CONSUMED % ring_bytes` whenever capture is ahead of
//!   consumption, parking until new capture arrives.  Partial reads carry over
//!   into the next call, so unread bytes are never discarded.
//!
//! The cursors are lock-free atomics and the DMA buffers are only touched by
//! their owning side, so playback, capture and full-duplex coexist with no
//! `Inner` mutex at runtime (the mutex is held only during init/bring-up).  A
//! single producer/consumer lock guards each direction against concurrent
//! callers (e.g. two user tasks staging playback at once); the ISR shares the
//! cursors lock-free.
//!
//! The completion ISR advances the cursors and zeroes consumed output slots.
//! If no interrupt route could be established the driver falls back to
//! servicing the BCIS latches from the waiting caller (polled), so playback
//! still progresses.
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

use super::AudioDevice;
use super::codec::{self, VerbSender};
use crate::drivers::serial::SerialPort;
use crate::services::dma::DmaAllocator;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// Output converter stream tag (matches SDnCTL bits 20..23).
const STREAM_TAG: u32 = 1;
/// Input converter stream tag.
const INPUT_TAG: u32 = 2;

// ── Feeding-ring geometry ───────────────────────────────────────────
/// Per-slot size in bytes.  4096 B = 1024 stereo frames ≈ 21 ms at 48 kHz.
/// Slots must keep the 128-byte buffer alignment of HDA 3.6.3 (both 128 and
/// 4096 satisfy it).
const RING_SLOT_BYTES: usize = 4096;
/// Ring depth: slots per direction.  16 slots ≈ 341 ms of total in-flight
/// DMA headroom at 48 kHz.
const RING_SLOTS: usize = 16;
/// One contiguous DMA buffer per direction: `RING_SLOT_BYTES * RING_SLOTS`.
const RING_BUF_BYTES: usize = RING_SLOT_BYTES * RING_SLOTS;

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
/// controller interrupt only while set.
const SD_CTL_IOCE: u32 = 1 << 2;
const SD_CTL_STREAM_TAG: u32 = 1 << 20; // stream tag 1
/// Input stream tag — matches the codec's ADC `SET_CONV` binding (tag 2).
const SD_CTL_INPUT_STREAM_TAG: u32 = 2 << 20;

/// Controller stream descriptor format: 16-bit stereo, 48 kHz base rate.
///
/// Stream-format structure (spec 3.7.1, Table 53), shared by SDnFMT and the
/// codec `SET_STREAM_FORMAT` verb: TYPE=0 (PCM) · BASE=0 (48 kHz) ·
/// MULT=000 (÷1) · DIV=000 (÷1) · BITS=001 (16-bit) · CHAN=0001 (2 ch)
/// ⇒ 0x0011.  This MUST agree with the codec-side verb
/// (`codec::STREAM_FMT_48K_STEREO_16` is also 0x11).
const SD_FMT_48K_STEREO_16: u16 = 0x0011;

const BDL_IOC: u32 = 0x01; // interrupt-on-complete flag

/// Stream-completion status bit (BCIS) within the SD_STS byte.
const SD_STS_BCIS: u8 = 0x04;
/// All Write-1-to-Clear bits of SD_STS (spec 3.3.36): BCIS (bit 2), FIFOE
/// (bit 3) and DESE (bit 5).  Clearing all of them on stream reset prevents a
/// stale error latch from raising spurious interrupts later.
const SD_STS_CLEAR_MASK: u8 = 0x04 | 0x08 | 0x20;

/// Timeout for stream SRST spin-loops (10 ms).  Real HDA controllers clear
/// SRST within a few hundred microseconds; QEMU transitions instantly.
const STREAM_RESET_TIMEOUT_NS: u64 = 10_000_000;

// ── Feeding-ring cursors and geometry (lock-free, shared with the ISR) ──
//
// The DMA buffers are owned by the ring; the ISR (or, in the polled fallback,
// the waiting caller) advances the completion cursors.  Geometry is published
// once at init so the ISR and callers agree on slot sizes.

static HDA_MMIO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Output stream descriptor base.
static HDA_OUT_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Input stream descriptor base.  0 = sentinel for "capture not armed".
static HDA_IN_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// True once a working MSI/INTx route exists (the ISR advances the cursors);
/// false → callers service the BCIS latches themselves.
static INTERRUPT_DRIVEN: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Output ring: bytes staged by callers (the DMA plays a whole slot per
/// completion, but a submission may stage any byte count).
static OUT_PRODUCED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Output ring: bytes the DMA has finished playing (ISR/poll advanced by a
/// full slot).
static OUT_COMPLETED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_BUF_VIRT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_SLOT_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_RING_SLOTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Serialises concurrent playback callers (the cursors alone would let two
/// tasks double-stage the same bytes).
static OUT_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Input ring: bytes the input DMA has filled (ISR/poll advanced by a full
/// slot).
static IN_CAPTURED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Input ring: bytes read out by callers.
static IN_CONSUMED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_BUF_VIRT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_SLOT_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_RING_SLOTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Serialises concurrent capture callers.
static IN_LOCK: spin::Mutex<()> = spin::Mutex::new(());
/// Serialises the polled BCIS read-clear-advance, so two waiting callers
/// (e.g. a playback task and a capture task) can never both observe one
/// completion latch and double-advance a cursor.  Only ever taken in polled
/// mode (the ISR path is the sole servicer when interrupt-driven), and always
/// innermost, so it cannot deadlock.
static POLL_LOCK: spin::Mutex<()> = spin::Mutex::new(());

use core::sync::atomic::Ordering;

/// Advance the output cursor by one slot and zero the just-consumed slot, so a
/// stalled producer plays silence (the DMA, on its wrap, reads a zeroed slot)
/// rather than a stale tail from an earlier lap.  Must be called exactly once
/// per output completion, either by the ISR or by a polled caller.  Uses
/// `fetch_add` so concurrent completions never lose an increment.
fn out_completed_advance() {
    let buf = OUT_BUF_VIRT.load(Ordering::Relaxed);
    let slot = OUT_SLOT_BYTES.load(Ordering::Relaxed) as usize;
    let slots = OUT_RING_SLOTS.load(Ordering::Relaxed) as usize;
    if buf == 0 || slot == 0 || slots == 0 {
        return;
    }
    let ring = slot * slots;
    let c = OUT_COMPLETED.fetch_add(slot as u64, Ordering::AcqRel);
    let pos = (c as usize) % ring;
    let dst = unsafe { (buf as *mut u8).add(pos) };
    unsafe {
        core::ptr::write_bytes(dst, 0, slot);
    }
}

/// Advance the input cursor by one slot (the DMA has filled a slot).
fn in_captured_advance() {
    let slot = IN_SLOT_BYTES.load(Ordering::Relaxed);
    if slot != 0 {
        let _ = IN_CAPTURED.fetch_add(slot, Ordering::Relaxed);
    }
}

/// Called from the device vector with interrupts disabled.  Acknowledges
/// every pending stream completion and advances that direction's cursor.
fn hda_irq_handler() {
    let mmio = HDA_MMIO.load(Ordering::Relaxed);
    if mmio == 0 {
        return;
    }
    let ob = HDA_OUT_BASE.load(Ordering::Relaxed) as u64;
    let ib = HDA_IN_BASE.load(Ordering::Relaxed) as u64;
    unsafe {
        if read_volatile((mmio + ob + regs::SD_STS as u64) as *const u8) & SD_STS_BCIS != 0 {
            write_volatile((mmio + ob + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
            out_completed_advance();
        }
        if ib != 0 {
            if read_volatile((mmio + ib + regs::SD_STS as u64) as *const u8) & SD_STS_BCIS != 0 {
                write_volatile((mmio + ib + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
                in_captured_advance();
            }
        }
    }
}

/// Polled fallback: when no interrupt route was established, a waiting caller
/// services the BCIS latches directly (mirroring the ISR) so the cursors
/// still advance and playback/capture progress.  No-op when interrupt-driven.
/// Serialised by `POLL_LOCK` so concurrent callers cannot both consume one
/// latch.
fn service_polled_completions() {
    if INTERRUPT_DRIVEN.load(Ordering::Relaxed) {
        return;
    }
    let _guard = POLL_LOCK.lock();
    let mmio = HDA_MMIO.load(Ordering::Relaxed);
    if mmio == 0 {
        return;
    }
    let ob = HDA_OUT_BASE.load(Ordering::Relaxed) as u64;
    let ib = HDA_IN_BASE.load(Ordering::Relaxed) as u64;
    unsafe {
        if read_volatile((mmio + ob + regs::SD_STS as u64) as *const u8) & SD_STS_BCIS != 0 {
            write_volatile((mmio + ob + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
            out_completed_advance();
        }
        if ib != 0 {
            if read_volatile((mmio + ib + regs::SD_STS as u64) as *const u8) & SD_STS_BCIS != 0 {
                write_volatile((mmio + ib + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
                in_captured_advance();
            }
        }
    }
}

/// Park the caller (cooperatively in task context, HLT in boot context) until
/// `done()` becomes true.  The condition is re-checked every ~1 ms slice, and
/// — in the polled BCIS fallback — the completion latches are serviced inside
/// the wait itself, so a completion is observed within one slice instead of
/// only when the 10 ms outer deadline expires.  (`service_polled_completions`
/// is a no-op in interrupt-driven mode, so wrapping the condition is free.)
fn ring_wait_until(done: &dyn Fn() -> bool) {
    while !done() {
        service_polled_completions();
        let check = || {
            service_polled_completions();
            done()
        };
        let _ = crate::services::universal_timer::wait_until_cond_coop(
            crate::services::universal_timer::now_ns() + 10_000_000,
            1_000_000,
            &check,
        );
    }
}

// ── Init-time stream helpers ────────────────────────────────────────

/// Reset a stream descriptor: clear RUN, assert SRST, deassert SRST, and
/// verify each phase completes before programming descriptors.  Returns
/// `Err("stream reset timeout")` if the controller does not settle.
fn reset_stream(mmio: u64, base: u32, tag: u32) -> Result<(), &'static str> {
    let r32 = |off: u32| unsafe { read_volatile((mmio + base as u64 + off as u64) as *const u32) };
    let w32 = |off: u32, v: u32| unsafe {
        write_volatile((mmio + base as u64 + off as u64) as *mut u32, v)
    };
    let w8 = |off: u32, v: u8| unsafe { write_volatile((mmio + base as u64 + off as u64) as *mut u8, v) };

    // Phase 1: Clear RUN and wait for the controller to report RUN=0 (3.3.35).
    // Asserting SRST while RUN is still set is an invalid state that can wedge
    // the stream DMA engine, so fail loudly rather than proceeding blind.
    w32(regs::SD_CTL, tag); // clear RUN, preserve tag
    let deadline1 = crate::services::universal_timer::now_ns() + STREAM_RESET_TIMEOUT_NS;
    if !crate::services::universal_timer::wait_until_cond(deadline1, &|| {
        r32(regs::SD_CTL) & SD_CTL_RUN == 0
    }) {
        return Err("stream reset timeout (RUN clear)");
    }

    // Phase 2: Assert SRST and verify that the controller entered reset.
    w32(regs::SD_CTL, tag | SD_CTL_SRST);
    let deadline2 = crate::services::universal_timer::now_ns() + STREAM_RESET_TIMEOUT_NS;
    if !crate::services::universal_timer::wait_until_cond(deadline2, &|| {
        r32(regs::SD_CTL) & SD_CTL_SRST != 0
    }) {
        return Err("stream reset timeout (SRST assert)");
    }

    // Phase 3: Deassert SRST explicitly.  QEMU does not self-clear this bit.
    w32(regs::SD_CTL, tag);
    let deadline3 = crate::services::universal_timer::now_ns() + STREAM_RESET_TIMEOUT_NS;
    if !crate::services::universal_timer::wait_until_cond(deadline3, &|| {
        r32(regs::SD_CTL) & SD_CTL_SRST == 0
    }) {
        return Err("stream reset timeout (SRST release)");
    }

    // Clear every Write-1-to-Clear status bit (BCIS, FIFOE, DESE) so a stale
    // error latch cannot fire spurious interrupts on the next stream run.
    w8(regs::SD_STS, SD_STS_CLEAR_MASK);
    Ok(())
}

struct Inner {
    mmio: u64,
    corb_phys: u64,
    corb_virt: u64,
    rirb_phys: u64,
    rirb_virt: u64,
    /// Output ring: BDL page + contiguous PCM buffer.
    out_bdl_phys: u64,
    out_bdl_virt: u64,
    out_buf_phys: u64,
    out_buf_virt: u64,
    /// Input ring: BDL page + contiguous PCM buffer.
    in_bdl_phys: u64,
    in_bdl_virt: u64,
    in_buf_phys: u64,
    in_buf_virt: u64,
    /// Register offset of the first output stream.
    out_base: u32,
    /// Register offset of the first input stream (0x80).  0 when capture is
    /// not armed.
    in_base: u32,
    /// RIRB write pointer seen so far (response ring consumption).
    last_wp: u16,
}

impl Inner {
    fn w8(&self, off: u32, v: u8) {
        unsafe { write_volatile((self.mmio + off as u64) as *mut u8, v) }
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
        self.w8(regs::RIRBSTS, RIRB_INT_MASK);
        self.drain_rirb(true);

        let mmio = self.mmio;
        let drain_deadline = crate::services::universal_timer::now_ns() + VERB_TIMEOUT_NS;
        if !crate::services::universal_timer::wait_until_cond(drain_deadline, &|| {
            let rp = (unsafe { read_volatile((mmio + regs::CORBRP as u64) as *const u16) }) & 0xff;
            let wp = (unsafe { read_volatile((mmio + regs::CORBWP as u64) as *const u16) }) & 0xff;
            rp == wp
        }) {
            return Err("CORB ring not empty");
        }

        let wp = self.r16(regs::CORBWP) & 0xff;
        let nxt = (wp + 1) & 0xff;
        unsafe { write_volatile((self.corb_virt + nxt as u64 * 4) as *mut u32, cmd) };
        self.w16(regs::CORBWP, nxt);

        let consumed_deadline = crate::services::universal_timer::now_ns() + VERB_TIMEOUT_NS;
        if !crate::services::universal_timer::wait_until_cond(consumed_deadline, &|| {
            let rp = (unsafe { read_volatile((mmio + regs::CORBRP as u64) as *const u16) }) & 0xff;
            rp == nxt
        }) {
            return Err("CORB command not consumed");
        }

        let rirb_virt = self.rirb_virt;
        let cad = (cmd >> 28) & 0xF;
        let mut last_wp = self.last_wp;
        let response_deadline = crate::services::universal_timer::now_ns() + VERB_TIMEOUT_NS;
        loop {
            let wp = (unsafe { read_volatile((mmio + regs::RIRBWP as u64) as *const u16) }) & 0xff;
            if wp == last_wp {
                let got =
                    crate::services::universal_timer::wait_until_cond(response_deadline, &|| {
                        let w =
                            (unsafe { read_volatile((mmio + regs::RIRBWP as u64) as *const u16) })
                                & 0xff;
                        w != last_wp
                    });
                if !got {
                    let wp_now =
                        (unsafe { read_volatile((mmio + regs::RIRBWP as u64) as *const u16) })
                            & 0xff;
                    let res =
                        unsafe { read_volatile((rirb_virt + wp_now as u64 * 8) as *const u32) };
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
            let mut idx = (last_wp + 1) & 0xff;
            let mut found: Option<u32> = None;
            while idx != ((wp + 1) & 0xff) {
                let res = unsafe { read_volatile((rirb_virt + idx as u64 * 8) as *const u32) };
                let res_ex =
                    unsafe { read_volatile((rirb_virt + idx as u64 * 8 + 4) as *const u32) };
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
    /// `last_wp` to the current RIRBWP.
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

    /// Program `slots` cyclic IOC BDL entries over the ring buffer, reset the
    /// stream, and start DMA.  Called once per direction at init; the ring
    /// then runs for the kernel's lifetime.
    fn start_ring(
        &self,
        base: u32,
        tag: u32,
        bdl_phys: u64,
        bdl_virt: u64,
        buf_phys: u64,
        slot: usize,
        slots: usize,
    ) -> Result<(), &'static str> {
        let bdl = bdl_virt as *mut u64;
        for k in 0..slots {
            unsafe {
                write_volatile(bdl.add(k * 2), buf_phys + (k as u64) * slot as u64);
                write_volatile(bdl.add(k * 2 + 1), ((BDL_IOC as u64) << 32) | (slot as u64));
            }
        }
        reset_stream(self.mmio, base, tag)?;
        self.w32(base + regs::SD_BDPL, bdl_phys as u32);
        self.w32(base + regs::SD_BDPU, (bdl_phys >> 32) as u32);
        self.w16(base + regs::SD_LVI, (slots - 1) as u16);
        self.w32(base + regs::SD_CBL, (slots * slot) as u32);
        self.w16(base + regs::SD_FMT, SD_FMT_48K_STEREO_16);
        self.w32(base + regs::SD_CTL, tag | SD_CTL_RUN | SD_CTL_IOCE);
        Ok(())
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

    fn submit_playback(&self, samples: &[i16]) -> Result<(), &'static str> {
        if samples.is_empty() {
            // A zero-length flush is a benign no-op, not an error.
            return Ok(());
        }
        if samples.len() % 2 != 0 {
            return Err("odd sample count (stereo requires even)");
        }
        let _guard = OUT_LOCK.lock();
        let nbytes = samples.len() * 2;
        let slot = OUT_SLOT_BYTES.load(Ordering::Relaxed) as usize;
        let slots = OUT_RING_SLOTS.load(Ordering::Relaxed) as usize;
        let ring = slot * slots;
        let buf_virt = OUT_BUF_VIRT.load(Ordering::Relaxed) as *mut u8;
        if slot == 0 || slots == 0 || buf_virt.is_null() {
            return Err("audio ring not initialised");
        }
        let mut off = 0usize;
        while off < nbytes {
            // Wait until the producer is less than a full ring ahead of the
            // play head: a full ring would alias the write back onto the DMA's
            // in-progress slot.  Signed distance, so a producer that is behind
            // the play head (e.g. the first call after the DMA has already
            // been running) proceeds immediately instead of wrapping forever.
            let ring_i = ring as i128;
            ring_wait_until(&|| {
                let p = OUT_PRODUCED.load(Ordering::Relaxed) as i128;
                let c = OUT_COMPLETED.load(Ordering::Acquire) as i128;
                p - c < ring_i
            });
            let p = OUT_PRODUCED.load(Ordering::Relaxed) as usize;
            let pos = p % ring;
            // Never cross a ring boundary in one write (a partial slot simply
            // carries over into the next call).
            let take = (nbytes - off).min(ring - pos);
            let dst = unsafe { buf_virt.add(pos) };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    samples.as_ptr().add(off / 2) as *const u8,
                    dst,
                    take,
                );
            }
            OUT_PRODUCED.store((p + take) as u64, Ordering::Release);
            off += take;
        }
        Ok(())
    }

    fn can_record(&self) -> bool {
        self.cap_ready.load(Ordering::Acquire)
    }

    fn read_capture(&self, dest: &mut [i16]) -> Result<(), &'static str> {
        if !self.cap_ready.load(Ordering::Acquire) {
            return Err("capture not supported");
        }
        if dest.is_empty() {
            // A zero-length read is a benign no-op, not an error.
            return Ok(());
        }
        if dest.len() % 2 != 0 {
            return Err("odd sample count (stereo requires even)");
        }
        let _guard = IN_LOCK.lock();
        let nbytes = dest.len() * 2;
        let slot = IN_SLOT_BYTES.load(Ordering::Relaxed) as usize;
        let slots = IN_RING_SLOTS.load(Ordering::Relaxed) as usize;
        let ring = slot * slots;
        let buf_virt = IN_BUF_VIRT.load(Ordering::Relaxed) as *const u8;
        if slot == 0 || slots == 0 || buf_virt.is_null() {
            return Err("audio ring not initialised");
        }
        let mut off = 0usize;
        while off < nbytes {
            // Wait until at least one captured byte is unconsumed.  Byte
            // distance, so a fully-wrapped ring (all slots captured) still
            // drains instead of deadlocking on the old modulo test.
            ring_wait_until(&|| {
                let cap = IN_CAPTURED.load(Ordering::Acquire);
                let con = IN_CONSUMED.load(Ordering::Relaxed);
                cap.wrapping_sub(con) > 0
            });
            let cap = IN_CAPTURED.load(Ordering::Acquire) as usize;
            let con = IN_CONSUMED.load(Ordering::Relaxed) as usize;
            let avail = cap.wrapping_sub(con);
            let pos = con % ring;
            // Never cross a ring boundary, and never read past what the input
            // DMA has actually filled (partial reads carry over into the next
            // call instead of discarding the rest of the slot).
            let take = (nbytes - off).min(avail).min(ring - pos);
            let src = unsafe { buf_virt.add(pos) };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src,
                    dest.as_mut_ptr().add(off / 2) as *mut u8,
                    take,
                );
            }
            IN_CONSUMED.store((con + take) as u64, Ordering::Release);
            off += take;
        }
        Ok(())
    }
}

/// Bring up the controller at `dev` and return it as a leakable device.
pub fn init(dev: &crate::pci::PciDevice) -> Result<&'static dyn AudioDevice, &'static str> {
    crate::pci::enable_device(dev);

    let base = match crate::pci::bar::bar(dev, 0) {
        crate::pci::bar::Bar::Memory { addr, .. } => addr,
        _ => return Err("HDA BAR0 is not memory-mapped"),
    };

    let dma: &dyn DmaAllocator = crate::services::kernel_services().dma;

    let mmio = dma.map_mmio(base, 0x4000)?;

    // Read the controller capabilities once, up front: input/output stream
    // counts and the 64-bit DMA support bit (GCAP bit 0, spec 3.3.1).  GCAP
    // is read-only, so this is valid before the controller reset.
    let gcap = unsafe { read_volatile((mmio + regs::GCAP as u64) as *const u16) };
    let gcap_64ok = gcap & 1 != 0;
    let iss = (gcap >> 8) & 0x0f;
    let oss = (gcap >> 12) & 0x0f;
    // Input descriptors occupy the first `iss` slots starting at 0x80; the
    // first output descriptor follows them.
    let out_base = 0x80 + (iss as u32) * 0x20;
    let in_base = 0x80u32;

    let corb = dma.alloc_page().ok_or("OOM CORB")?;
    let rirb = dma.alloc_page().ok_or("OOM RIRB")?;
    // Output ring: a BDL page (16 entries fit trivially) and a contiguous PCM
    // buffer of `RING_BUF_BYTES`.
    let out_bdl = dma.alloc_page().ok_or("OOM output BDL")?;
    let out_buf = dma
        .alloc_contiguous(RING_BUF_BYTES / 4096)
        .ok_or("OOM output ring buffer")?;

    // On a 32-bit-only controller (64OK == 0) the upper-base registers are
    // ignored, so a DMA address above 4 GiB would silently corrupt low
    // memory.  Fail bring-up rather than risk it.
    if !gcap_64ok
        && (corb.phys >= 0x1_0000_0000
            || rirb.phys >= 0x1_0000_0000
            || out_bdl.phys >= 0x1_0000_0000
            || out_buf.phys >= 0x1_0000_0000)
    {
        return Err("HDA controller is 32-bit only and a DMA buffer sits above 4 GiB");
    }

    let audio = Box::new(HdaAudio {
        inner: Mutex::new(Inner {
            mmio,
            corb_phys: corb.phys,
            corb_virt: corb.virt,
            rirb_phys: rirb.phys,
            rirb_virt: rirb.virt,
            out_bdl_phys: out_bdl.phys,
            out_bdl_virt: out_bdl.virt,
            out_buf_phys: out_buf.phys,
            out_buf_virt: out_buf.virt,
            in_bdl_phys: 0,
            in_bdl_virt: 0,
            in_buf_phys: 0,
            in_buf_virt: 0,
            out_base: 0,
            in_base: 0,
            last_wp: 0,
        }),
        cap_ready: core::sync::atomic::AtomicBool::new(false),
    });
    let audio: &'static HdaAudio = Box::leak(audio);

    {
        let mut i = audio.inner.lock();

        // Controller reset.  Per the HDA spec (§3.3.7) the exit-reset phase
        // needs at least 521 µs of CRST-asserted time; a bare spin loop turns
        // over in microseconds on modern CPUs, so wait on the timer and fail
        // loudly if the controller never settles.
        SerialPort::puts("[audio] hda: controller reset\n");
        i.w32(regs::GCTL, 0);
        let reset_clear_deadline = crate::services::universal_timer::now_ns() + 100_000_000;
        if !crate::services::universal_timer::wait_until_cond(reset_clear_deadline, &|| {
            i.r32(regs::GCTL) & GCTL_RSTCRST == 0
        }) {
            return Err("HDA controller reset timeout (CRST clear)");
        }
        i.w32(regs::GCTL, GCTL_RSTCRST);
        let reset_set_deadline = crate::services::universal_timer::now_ns() + 100_000_000;
        if !crate::services::universal_timer::wait_until_cond(reset_set_deadline, &|| {
            i.r32(regs::GCTL) & GCTL_RSTCRST != 0
        }) {
            return Err("HDA controller reset timeout (CRST set)");
        }

        let c_mmio = i.mmio;
        let present_deadline = crate::services::universal_timer::now_ns() + 100_000_000;
        crate::services::universal_timer::wait_until_cond(present_deadline, &|| {
            (unsafe { read_volatile((c_mmio + regs::STATESTS as u64) as *const u16) }) != 0
        });
        let sts = i.r16(regs::STATESTS);
        // STATESTS carries write-1-to-clear SDI wake bits (spec §3.3.9);
        // write the latched value back so stale wake flags are acknowledged
        // instead of hanging around.
        i.w16(regs::STATESTS, sts);

        i.out_base = out_base;
        SerialPort::puts("[audio] hda: iss=");
        SerialPort::put_u64(iss as u64);
        SerialPort::puts(" oss=");
        SerialPort::put_u64(oss as u64);
        SerialPort::puts(" 64ok=");
        SerialPort::put_u64(gcap_64ok as u64);
        SerialPort::puts(" out_base=0x");
        SerialPort::put_hex(out_base as u64);
        SerialPort::puts("\n");

        // CORB: base pointers, ring size (256 entries), reset pointers, run.
        // The read-pointer reset bit (bit 15, CORBRPRST, spec §3.3.20) must
        // read back asserted before it is cleared, or the controller has not
        // actually reset the pointer.  Some QEMU models never latch the bit,
        // so a timeout is logged and tolerated rather than aborting init.
        i.w32(regs::CORBLBASE, i.corb_phys as u32);
        i.w32(regs::CORBUBASE, (i.corb_phys >> 32) as u32);
        i.w8(regs::CORBSIZE, 0x02);
        i.w16(regs::CORBRP, 0x8000);
        let corb_rst_deadline = crate::services::universal_timer::now_ns() + 100_000_000;
        if !crate::services::universal_timer::wait_until_cond(corb_rst_deadline, &|| {
            i.r16(regs::CORBRP) & 0x8000 != 0
        }) {
            SerialPort::puts("[audio] hda: CORBRP reset bit never asserted\n");
        }
        i.w16(regs::CORBRP, 0);
        i.w16(regs::CORBWP, 0);
        i.w8(regs::CORBSTS, 0);
        i.w8(regs::CORBCTL, CORB_RUN);

        // RIRB: base pointers, ring size (256 entries), reset pointers, then
        // enable DMA.  RINTCNT must be non-zero and RIRBCTL must set DMA_EN.
        i.w32(regs::RIRBLBASE, i.rirb_phys as u32);
        i.w32(regs::RIRBUBASE, (i.rirb_phys >> 32) as u32);
        i.w8(regs::RIRBSIZE, 0x02);
        i.w16(regs::RIRBWP, 0x8000);
        let rirb_rst_deadline = crate::services::universal_timer::now_ns() + 100_000_000;
        if !crate::services::universal_timer::wait_until_cond(rirb_rst_deadline, &|| {
            i.r16(regs::RIRBWP) & 0x8000 != 0
        }) {
            SerialPort::puts("[audio] hda: RIRBWP reset bit never asserted\n");
        }
        i.w16(regs::RIRBWP, 0);
        i.w8(regs::RIRBSTS, 0);
        i.w16(regs::RINTCNT, RINTCNT_QUIET);
        i.w8(regs::RIRBCTL, RIRB_CTL);
        i.last_wp = 0;

        // Ping each present codec until its vendor ID reads back stable.
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

        SerialPort::puts("[audio] hda: states=0x");
        SerialPort::put_hex(sts as u64);
        SerialPort::puts("\n");

        let mut codec: Option<codec::Codec> = None;
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

        if codec::is_realtek_alc256(codec.vendor) {
            SerialPort::puts("[audio] hda: alc256 hardcoded analog path\n");
            codec::setup_alc256_output(&mut *i, &codec, STREAM_TAG)?;
        } else {
            codec::setup_output(&mut *i, &codec, STREAM_TAG)?;
        }
        // Capture needs both a codec ADC *and* an input stream descriptor on
        // the controller (ISS > 0).  On a playback-only controller the input
        // descriptor at 0x80 would alias the output stream, so never arm it.
        let mut cap_ok = iss > 0 && codec.adc.is_some();
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
        audio
            .cap_ready
            .store(cap_ok, core::sync::atomic::Ordering::Release);

        // Start the permanent output ring (silence until data is submitted).
        unsafe { core::ptr::write_bytes(i.out_buf_virt as *mut u8, 0, RING_BUF_BYTES) }
        i.start_ring(
            out_base,
            SD_CTL_STREAM_TAG,
            i.out_bdl_phys,
            i.out_bdl_virt,
            i.out_buf_phys,
            RING_SLOT_BYTES,
            RING_SLOTS,
        )?;
        OUT_PRODUCED.store(0, Ordering::Release);
        OUT_COMPLETED.store(0, Ordering::Release);
        OUT_BUF_VIRT.store(i.out_buf_virt, Ordering::Release);
        OUT_SLOT_BYTES.store(RING_SLOT_BYTES as u64, Ordering::Release);
        OUT_RING_SLOTS.store(RING_SLOTS as u64, Ordering::Release);
        SerialPort::puts("[audio] hda: output ring running\n");

        // When the codec captures, start the permanent input ring.
        if cap_ok {
            let in_bdl = dma.alloc_page().ok_or("OOM input BDL")?;
            let in_buf = dma
                .alloc_contiguous(RING_BUF_BYTES / 4096)
                .ok_or("OOM input ring buffer")?;
            if !gcap_64ok
                && (in_bdl.phys >= 0x1_0000_0000 || in_buf.phys >= 0x1_0000_0000)
            {
                return Err("HDA controller is 32-bit only and a DMA buffer sits above 4 GiB");
            }
            i.in_base = in_base;
            i.in_bdl_phys = in_bdl.phys;
            i.in_bdl_virt = in_bdl.virt;
            i.in_buf_phys = in_buf.phys;
            i.in_buf_virt = in_buf.virt;
            unsafe { core::ptr::write_bytes(i.in_buf_virt as *mut u8, 0, RING_BUF_BYTES) }
            i.start_ring(
                in_base,
                SD_CTL_INPUT_STREAM_TAG,
                i.in_bdl_phys,
                i.in_bdl_virt,
                i.in_buf_phys,
                RING_SLOT_BYTES,
                RING_SLOTS,
            )?;
            IN_CAPTURED.store(0, Ordering::Release);
            IN_CONSUMED.store(0, Ordering::Release);
            IN_BUF_VIRT.store(i.in_buf_virt, Ordering::Release);
            IN_SLOT_BYTES.store(RING_SLOT_BYTES as u64, Ordering::Release);
            IN_RING_SLOTS.store(RING_SLOTS as u64, Ordering::Release);
            SerialPort::puts("[audio] hda: input ring running\n");
        }

        // Publish the registers the completion ISR needs (lock-free), then
        // enable stream-completion interrupts.
        HDA_MMIO.store(mmio, Ordering::Release);
        HDA_OUT_BASE.store(out_base, Ordering::Release);
        if cap_ok {
            HDA_IN_BASE.store(in_base, Ordering::Release);
        }
        #[cfg(target_arch = "x86_64")]
        setup_stream_interrupt(dev, out_base, in_base, cap_ok);
    }

    Ok(audio)
}

/// Enable stream-completion interrupts for the output (and, when capture is
/// armed, input) streams: MSI when the controller exposes a capability,
/// legacy INTx otherwise.  Sets `INTERRUPT_DRIVEN` when a route succeeds; if
/// none can be established the driver falls back to polling BCIS from the
/// callers.
#[cfg(target_arch = "x86_64")]
fn setup_stream_interrupt(dev: &crate::pci::PciDevice, out_base: u32, in_base: u32, cap_ok: bool) {
    use crate::arch::x86_64::idt;
    use crate::drivers::serial::SerialPort;
    use crate::pci::caps;

    let stream_index = (out_base - 0x80) / 0x20;

    let caps_list = caps::all(dev);
    let mut route_ok = false;
    if let Some(msi) = caps_list.iter().find(|c| c.id == caps::CAP_MSI) {
        // MSI: the driver owns the vector, so grab a free device vector and
        // program MSI to deliver on it.
        let Some(vector) = idt::register_device_handler(hda_irq_handler) else {
            SerialPort::puts("[audio] hda: no device vector free, polling BCIS\n");
            INTERRUPT_DRIVEN.store(false, Ordering::Release);
            return;
        };
        let bsp_apic_id = unsafe {
            let lapic = crate::platform::x86_64_pc::apic::lapic_base();
            core::ptr::read_volatile((lapic as *const u32).add(0x20 / 4)) >> 24
        } as u8;
        crate::pci::msi::enable(dev, msi, vector, bsp_apic_id);
        SerialPort::puts("[audio] hda: MSI enabled\n");
        route_ok = true;
    } else if dev.interrupt_line != 0 {
        // INTx: the I/O APIC assigns the vector, so register the handler at
        // that vector rather than a separately-allocated one (the two pools
        // are independent and must not be assumed to coincide).
        if let Some(vector) = crate::platform::x86_64_pc::ioapic::enable_irq(
            dev.interrupt_line as u32,
            crate::acpi::Polarity::ActiveLow,
            crate::acpi::TriggerMode::Level,
        ) {
            idt::register_device_handler_at(vector, hda_irq_handler);
            SerialPort::puts("[audio] hda: INTx enabled\n");
            route_ok = true;
        }
    }
    if !route_ok {
        SerialPort::puts("[audio] hda: no interrupt source, polling BCIS\n");
        INTERRUPT_DRIVEN.store(false, Ordering::Release);
        return;
    }
    INTERRUPT_DRIVEN.store(true, Ordering::Release);

    // Gate completion interrupts for the output and (when armed) input
    // streams, plus the global enable.
    let mut intctl = (1 << stream_index) | (1 << 31);
    if cap_ok {
        let in_stream_index = (in_base - 0x80) / 0x20;
        intctl |= 1 << in_stream_index;
    }
    unsafe {
        let mmio = HDA_MMIO.load(Ordering::Relaxed);
        core::ptr::write_volatile((mmio + global_regs::INTCTL as u64) as *mut u32, intctl);
    }
}
