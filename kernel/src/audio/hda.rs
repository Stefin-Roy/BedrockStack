//! Intel HD Audio (ICH6/ICH9) controller driver.
//!
//! Polled/IRQ driver for QEMU's `intel-hda` / `ich9-intel-hda` emulation.  The
//! controller moves verbs to the codec over the CORB/RIRB rings and plays
//! 16-bit signed stereo PCM at 48 kHz through the codec's output converter
//! (discovered generically by `super::codec`).  When the chosen codec also
//! exposes an input path (e.g. QEMU's `hda-duplex`), the same ring machinery
//! drives the input converter the other way for capture.

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
/// Per-slot size in bytes. 2048 B = 512 stereo frames ≈ 10.7 ms at 48 kHz.
const RING_SLOT_BYTES: usize = 2048;
/// Ring depth: 8 slots ≈ 85.3 ms total buffer capacity.
const RING_SLOTS: usize = 8;
/// One contiguous DMA buffer per direction: 16,384 bytes.
const RING_BUF_BYTES: usize = RING_SLOT_BYTES * RING_SLOTS;

/// Producer staging cap (7 slots ≈ 74.6 ms of staged audio).
/// Leaves 1 slot of wrap clearance behind the write head.
const MAX_STAGED_BYTES: usize = RING_BUF_BYTES - RING_SLOT_BYTES;

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
    // Stream descriptor sub-offsets (relative to stream base).
    pub const SD_CTL: u32 = 0x00;
    pub const SD_STS: u32 = 0x03;
    pub const SD_LPIB: u32 = 0x04;
    pub const SD_CBL: u32 = 0x08;
    pub const SD_LVI: u32 = 0x0C;
    pub const SD_FMT: u32 = 0x12;
    pub const SD_BDPL: u32 = 0x18;
    pub const SD_BDPU: u32 = 0x1C;
}

mod global_regs {
    pub const INTCTL: u32 = 0x20;
}

const GCTL_RSTCRST: u32 = 1;
const CORB_RUN: u8 = 1 << 1;
const RIRB_CTL: u8 = (1 << 0) | (1 << 1);
const RIRB_INT_MASK: u8 = 0x07;
const RINTCNT_QUIET: u16 = 0xFF;
const VERB_TIMEOUT_NS: u64 = 100_000_000;

const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;
const SD_CTL_STREAM_TAG: u32 = 1 << 20;
const SD_CTL_INPUT_STREAM_TAG: u32 = 2 << 20;

const SD_FMT_48K_STEREO_16: u16 = 0x0011;
const BDL_IOC: u32 = 0x01;
const SD_STS_BCIS: u8 = 0x04;
const SD_STS_CLEAR_MASK: u8 = 0x04 | 0x08 | 0x20;
const STREAM_RESET_TIMEOUT_NS: u64 = 10_000_000;

// ── Feeding-ring cursors (lock-free, shared with ISR) ───────────────

static HDA_MMIO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HDA_OUT_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static HDA_IN_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static INTERRUPT_DRIVEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

static OUT_PRODUCED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_COMPLETED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LAST_OUT_LPIB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static OUT_BUF_VIRT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_SLOT_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_RING_SLOTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static OUT_LOCK: spin::Mutex<()> = spin::Mutex::new(());

static IN_CAPTURED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_CONSUMED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LAST_IN_LPIB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static IN_BUF_VIRT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_SLOT_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_RING_SLOTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IN_LOCK: spin::Mutex<()> = spin::Mutex::new(());
static POLL_LOCK: spin::Mutex<()> = spin::Mutex::new(());

use core::sync::atomic::Ordering;

fn out_completed_reconcile() {
    let buf = OUT_BUF_VIRT.load(Ordering::Relaxed);
    let slot = OUT_SLOT_BYTES.load(Ordering::Relaxed) as usize;
    let slots = OUT_RING_SLOTS.load(Ordering::Relaxed) as usize;
    let mmio = HDA_MMIO.load(Ordering::Relaxed);
    let ob = HDA_OUT_BASE.load(Ordering::Relaxed) as u64;
    if buf == 0 || slot == 0 || slots == 0 || mmio == 0 {
        return;
    }
    let ring = slot * slots;
    let lpib = unsafe {
        read_volatile((mmio + ob + regs::SD_LPIB as u64) as *const u32) as usize % ring
    };

    let prev_lpib = LAST_OUT_LPIB.swap(lpib as u32, Ordering::AcqRel) as usize;
    let delta = (lpib + ring - prev_lpib) % ring;
    if delta == 0 {
        return;
    }

    // Zero out precisely the range the DMA just read, preventing stale loops on underrun
    let base = buf as *mut u8;
    unsafe {
        if prev_lpib + delta <= ring {
            core::ptr::write_bytes(base.add(prev_lpib), 0, delta);
        } else {
            let first = ring - prev_lpib;
            core::ptr::write_bytes(base.add(prev_lpib), 0, first);
            core::ptr::write_bytes(base, 0, delta - first);
        }
    }

    OUT_COMPLETED.fetch_add(delta as u64, Ordering::Release);
}

fn in_captured_reconcile() {
    let slot = IN_SLOT_BYTES.load(Ordering::Relaxed) as usize;
    let slots = IN_RING_SLOTS.load(Ordering::Relaxed) as usize;
    let mmio = HDA_MMIO.load(Ordering::Relaxed);
    let ib = HDA_IN_BASE.load(Ordering::Relaxed) as u64;
    if slot == 0 || slots == 0 || mmio == 0 || ib == 0 {
        return;
    }
    let ring = slot * slots;
    let lpib = unsafe {
        read_volatile((mmio + ib + regs::SD_LPIB as u64) as *const u32) as usize % ring
    };

    let prev_lpib = LAST_IN_LPIB.swap(lpib as u32, Ordering::AcqRel) as usize;
    let delta = (lpib + ring - prev_lpib) % ring;
    if delta > 0 {
        IN_CAPTURED.fetch_add(delta as u64, Ordering::Release);
    }
}

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
        }
        out_completed_reconcile();
        if ib != 0 {
            if read_volatile((mmio + ib + regs::SD_STS as u64) as *const u8) & SD_STS_BCIS != 0 {
                write_volatile((mmio + ib + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
            }
            in_captured_reconcile();
        }
    }
}

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
        }
        out_completed_reconcile();
        if ib != 0 {
            if read_volatile((mmio + ib + regs::SD_STS as u64) as *const u8) & SD_STS_BCIS != 0 {
                write_volatile((mmio + ib + regs::SD_STS as u64) as *mut u8, SD_STS_BCIS);
            }
            in_captured_reconcile();
        }
    }
}

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

fn reset_stream(mmio: u64, base: u32, tag: u32) -> Result<(), &'static str> {
    let r32 = |off: u32| unsafe { read_volatile((mmio + base as u64 + off as u64) as *const u32) };
    let w32 = |off: u32, v: u32| unsafe {
        write_volatile((mmio + base as u64 + off as u64) as *mut u32, v)
    };
    let w8 = |off: u32, v: u8| unsafe { write_volatile((mmio + base as u64 + off as u64) as *mut u8, v) };

    w32(regs::SD_CTL, tag);
    let deadline1 = crate::services::universal_timer::now_ns() + STREAM_RESET_TIMEOUT_NS;
    if !crate::services::universal_timer::wait_until_cond(deadline1, &|| {
        r32(regs::SD_CTL) & SD_CTL_RUN == 0
    }) {
        return Err("stream reset timeout (RUN clear)");
    }

    w32(regs::SD_CTL, tag | SD_CTL_SRST);
    let deadline2 = crate::services::universal_timer::now_ns() + STREAM_RESET_TIMEOUT_NS;
    if !crate::services::universal_timer::wait_until_cond(deadline2, &|| {
        r32(regs::SD_CTL) & SD_CTL_SRST != 0
    }) {
        return Err("stream reset timeout (SRST assert)");
    }

    w32(regs::SD_CTL, tag);
    let deadline3 = crate::services::universal_timer::now_ns() + STREAM_RESET_TIMEOUT_NS;
    if !crate::services::universal_timer::wait_until_cond(deadline3, &|| {
        r32(regs::SD_CTL) & SD_CTL_SRST == 0
    }) {
        return Err("stream reset timeout (SRST release)");
    }

    w8(regs::SD_STS, SD_STS_CLEAR_MASK);
    Ok(())
}

struct Inner {
    mmio: u64,
    corb_phys: u64,
    corb_virt: u64,
    rirb_phys: u64,
    rirb_virt: u64,
    out_bdl_phys: u64,
    out_bdl_virt: u64,
    out_buf_phys: u64,
    out_buf_virt: u64,
    in_bdl_phys: u64,
    in_bdl_virt: u64,
    in_buf_phys: u64,
    in_buf_virt: u64,
    out_base: u32,
    in_base: u32,
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
            SerialPort::puts("\n");
        }
        self.last_wp = wp;
        n
    }

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
            let mut p = OUT_PRODUCED.load(Ordering::Relaxed);
            let c = OUT_COMPLETED.load(Ordering::Acquire);

            // Cold start or buffer underrun: DMA caught up to write head.
            // Realign directly to c so playback starts immediately without gaps.
            if p <= c {
                p = c;
                OUT_PRODUCED.store(p, Ordering::Relaxed);
            }

            let ahead = p - c;
            if ahead >= MAX_STAGED_BYTES as u64 {
                ring_wait_until(&|| {
                    let p2 = OUT_PRODUCED.load(Ordering::Relaxed);
                    let c2 = OUT_COMPLETED.load(Ordering::Acquire);
                    p2.saturating_sub(c2) < MAX_STAGED_BYTES as u64
                });
                continue;
            }

            let pos = (p as usize) % ring;
            let space_available = MAX_STAGED_BYTES - (ahead as usize);
            let take = (nbytes - off).min(ring - pos).min(space_available);
            let dst = unsafe { buf_virt.add(pos) };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    samples.as_ptr().add(off / 2) as *const u8,
                    dst,
                    take,
                );
            }
            OUT_PRODUCED.store(p + take as u64, Ordering::Release);
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
            ring_wait_until(&|| {
                let cap = IN_CAPTURED.load(Ordering::Acquire);
                let con = IN_CONSUMED.load(Ordering::Relaxed);
                cap > con
            });

            let cap = IN_CAPTURED.load(Ordering::Acquire);
            let mut con = IN_CONSUMED.load(Ordering::Relaxed);
            let avail = cap.saturating_sub(con) as usize;

            // Overrun: consumer lagged behind more than the full ring buffer
            if avail >= ring {
                con = cap - (slot as u64);
                IN_CONSUMED.store(con, Ordering::Relaxed);
                continue;
            }

            let pos = (con as usize) % ring;
            let take = (nbytes - off).min(avail).min(ring - pos);
            let src = unsafe { buf_virt.add(pos) };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src,
                    dest.as_mut_ptr().add(off / 2) as *mut u8,
                    take,
                );
            }
            IN_CONSUMED.store(con + take as u64, Ordering::Release);
            off += take;
        }
        Ok(())
    }
}

pub fn init(dev: &crate::pci::PciDevice) -> Result<&'static dyn AudioDevice, &'static str> {
    crate::pci::enable_device(dev);

    let base = match crate::pci::bar::bar(dev, 0) {
        crate::pci::bar::Bar::Memory { addr, .. } => addr,
        _ => return Err("HDA BAR0 is not memory-mapped"),
    };

    let dma: &dyn DmaAllocator = crate::services::kernel_services().dma;
    let mmio = dma.map_mmio(base, 0x4000)?;

    let gcap = unsafe { read_volatile((mmio + regs::GCAP as u64) as *const u16) };
    let gcap_64ok = gcap & 1 != 0;
    let iss = (gcap >> 8) & 0x0f;
    let oss = (gcap >> 12) & 0x0f;
    let out_base = 0x80 + (iss as u32) * 0x20;
    let in_base = 0x80u32;

    let corb = dma.alloc_page().ok_or("OOM CORB")?;
    let rirb = dma.alloc_page().ok_or("OOM RIRB")?;
    let out_bdl = dma.alloc_page().ok_or("OOM output BDL")?;
    let out_buf = dma
        .alloc_contiguous(RING_BUF_BYTES / 4096)
        .ok_or("OOM output ring buffer")?;

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

        i.w32(regs::RIRBLBASE, i.rirb_phys as u32);
        i.w32(regs::RIRBUBASE, (i.rirb_phys >> 32) as u32);
        i.w8(regs::RIRBSIZE, 0x02);
        i.w16(regs::RIRBWP, 0x8000);
        // RIRBWPRST is write-only (spec 3.3.27: "always read as 0"), so the reset
        // cannot be verified by read-back; just clear the reset bit again.
        i.w16(regs::RIRBWP, 0);
        i.w8(regs::RIRBSTS, 0);
        i.w16(regs::RINTCNT, RINTCNT_QUIET);
        i.w8(regs::RIRBCTL, RIRB_CTL);
        i.last_wp = 0;

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
                            break;
                        }
                    }
                }
            }
        }

        let mut codec: Option<codec::Codec> = None;
        let mut duplex: Option<codec::Codec> = None;
        let mut digital: Option<codec::Codec> = None;
        for cad in 0..16u32 {
            if sts & (1 << cad) == 0 {
                continue;
            }
            if let Ok(c) = codec::probe(&mut *i, cad) {
                if codec::is_realtek_alc256(c.vendor) {
                    if codec.is_none() { codec = Some(c); }
                } else if c.dac.is_some() {
                    if c.out_is_analog() {
                        if c.adc.is_some() {
                            if duplex.is_none() { duplex = Some(c); }
                        } else if codec.is_none() {
                            codec = Some(c);
                        }
                    } else if digital.is_none() {
                        digital = Some(c);
                    }
                }
            }
        }
        let codec = duplex.or(codec).or(digital).ok_or("no usable codec")?;

        if codec::is_realtek_alc256(codec.vendor) {
            codec::setup_alc256_output(&mut *i, &codec, STREAM_TAG)?;
        } else {
            codec::setup_output(&mut *i, &codec, STREAM_TAG)?;
        }

        let mut cap_ok = iss > 0 && codec.adc.is_some();
        if cap_ok {
            if let Err(_) = codec::setup_input(&mut *i, &codec, INPUT_TAG) {
                cap_ok = false;
            }
        }
        audio.cap_ready.store(cap_ok, Ordering::Release);

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
        LAST_OUT_LPIB.store(0, Ordering::Release);
        OUT_BUF_VIRT.store(i.out_buf_virt, Ordering::Release);
        OUT_SLOT_BYTES.store(RING_SLOT_BYTES as u64, Ordering::Release);
        OUT_RING_SLOTS.store(RING_SLOTS as u64, Ordering::Release);

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
            LAST_IN_LPIB.store(0, Ordering::Release);
            IN_BUF_VIRT.store(i.in_buf_virt, Ordering::Release);
            IN_SLOT_BYTES.store(RING_SLOT_BYTES as u64, Ordering::Release);
            IN_RING_SLOTS.store(RING_SLOTS as u64, Ordering::Release);
        }

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

#[cfg(target_arch = "x86_64")]
fn setup_stream_interrupt(dev: &crate::pci::PciDevice, out_base: u32, in_base: u32, cap_ok: bool) {
    use crate::arch::x86_64::idt;
    use crate::drivers::serial::SerialPort;
    use crate::pci::caps;

    let stream_index = (out_base - 0x80) / 0x20;
    let caps_list = caps::all(dev);
    let mut route_ok = false;
    if let Some(msi) = caps_list.iter().find(|c| c.id == caps::CAP_MSI) {
        let Some(vector) = idt::register_device_handler(hda_irq_handler) else {
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
        INTERRUPT_DRIVEN.store(false, Ordering::Release);
        return;
    }
    INTERRUPT_DRIVEN.store(true, Ordering::Release);

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