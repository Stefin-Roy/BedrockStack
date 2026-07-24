//! AHCI (Advanced Host Controller Interface) SATA driver.
//!
//! Polling-mode driver for the Q35 ICH9 AHCI controller with interrupt
//! support via the pre-registered device interrupt vectors (33-48).
//!
//! Features:
//!   - NCQ (Native Command Queuing) via FPDMA QUEUED (0x60/0x61)
//!   - Pre-allocated per-slot Command Table pages
//!   - Translation cache to avoid repeated 4-level page walks
//!   - Proper timeout via APIC timer count
//!   - TFD error checking + SERR diagnostics
//!   - Port reset recovery on command failure
//!   - Zero-copy DMA: PRDT points directly to caller buffer pages
//!   - Multi-PRDT for large transfers (up to 64 pages)
//!   - u32 FIS writes for lower overhead
//!   - Proper BAR type detection (32/64-bit MMIO)
//!   - Proper MMIO region sizing from CAP.NP
//!   - Interrupt-driven completion with polling fallback
//!   - Multi-port and multi-controller support

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::AtomicU32;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::platform::x86_64_pc::apic;
use super::dma;
use super::dma::DmaAllocator;
use super::driver::StorageDriver;
use super::traits::{BlockDevice, IoRequest, IoBuffer, IoCompletions};

const AHCI_MAX_SLOTS: usize = 32;
const MAX_PRDT: usize = 64;

#[allow(dead_code)]
mod ghc {
    pub const CAP: u32 = 0x00;
    pub const GHC: u32 = 0x04;
    pub const IS: u32 = 0x08;
    pub const PI: u32 = 0x0C;
    pub const VS: u32 = 0x10;
}

mod port_off {
    pub const CLB: u32 = 0x00;
    pub const CLBU: u32 = 0x04;
    pub const FB: u32 = 0x08;
    pub const FBU: u32 = 0x0C;
    pub const IS: u32 = 0x10;
    pub const IE: u32 = 0x14;
    pub const CMD: u32 = 0x18;
    pub const TFD: u32 = 0x20;
    pub const SIG: u32 = 0x24;
    pub const SSTS: u32 = 0x28;
    pub const SCTL: u32 = 0x2C;
    pub const SERR: u32 = 0x30;
    pub const SACT: u32 = 0x34;
    pub const CI: u32 = 0x38;
}

const GHC_HR: u32 = 1;
const GHC_AE: u32 = 1 << 31;

const CMD_ST: u32 = 1;
const CMD_FRE: u32 = 1 << 4;
const CMD_CR: u32 = 1 << 15;
const CMD_FR: u32 = 1 << 14;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;

const SSTS_DET_MASK: u32 = 0x0F;
const SSTS_DET_ESTAB: u32 = 3;

const TFD_ERR: u32 = 1 << 0;

// The LAPIC timer normally provides the wall-clock deadline for AHCI polling.
// Do not rely on it exclusively, though: a stopped/misconfigured timer used to
// leave the BSP spinning forever during controller discovery.  This is a
// deliberately generous last-resort bound; normal QEMU hardware completes
// these operations long before it is reached.
const POLL_FALLBACK_LIMIT: u32 = 1_000_000;

#[derive(Clone, Copy)]
struct Hba { vaddr: u64 }

impl Hba {
    fn r32(self, off: u32) -> u32 {
        unsafe { read_volatile((self.vaddr + off as u64) as *const u32) }
    }
    fn w32(self, off: u32, v: u32) {
        unsafe { write_volatile((self.vaddr + off as u64) as *mut u32, v) }
    }
    fn pr32(self, p: u8, off: u32) -> u32 {
        self.r32(0x100 + (p as u32) * 0x80 + off)
    }
    fn pw32(self, p: u8, off: u32, v: u32) {
        self.w32(0x100 + (p as u32) * 0x80 + off, v)
    }
}

#[repr(C, packed)]
struct CmdHeader {
    cfl_w_prdtl: u32,
    prdbc: u32,
    ctba: u32,
    ctbau: u32,
    _rsvd: [u32; 4],
}

#[repr(C, packed)]
struct PrdEntry {
    dba: u32,
    dbau: u32,
    _rsvd: u32,
    dbc: u32,
}

#[derive(Clone, Copy)]
struct Slot {
    ct_paddr: u64,
    ct_vaddr: u64,
}

// ── APIC timer helpers ──────────────────────────────────────────

fn init_count() -> u32 { apic::timer_init_count() }
fn curr_count() -> u32 { apic::timer_current_count() }

fn elapsed_ticks(start: u32, wraps: u32) -> u32 {
    let i = init_count();
    let c = curr_count();
    if wraps == 0 {
        start.wrapping_sub(c)
    } else {
        start.wrapping_add(wraps.wrapping_mul(i + 1)).wrapping_sub(c)
    }
}

fn ticks_to_ms(t: u32) -> u32 {
    let (i, p) = (init_count(), apic::TIMER_PERIOD_MS);
    if i == 0 { return 0; }
    (t as u64 * p as u64 / i as u64) as u32
}

fn ms_to_ticks(ms: u32) -> u32 {
    let (i, p) = (init_count(), apic::TIMER_PERIOD_MS);
    if i == 0 { return 10_000_000; }
    (ms as u64 * i as u64 / p as u64) as u32
}

/// True when either the LAPIC deadline has passed or its counter is not
/// progressing.  The latter guard makes every MMIO polling loop finite even
/// when timer setup failed.
///
/// `wraps` counts how many times the periodic down-counter has wrapped past
/// zero, which is needed because the APIC timer runs in periodic mode and the
/// counter alone can only represent ~1 ms of elapsed time.
fn poll_timed_out(start: u32, deadline: u32, wraps: &mut u32, previous: &mut u32, stagnant: &mut u32) -> bool {
    let current = curr_count();
    if current == *previous {
        *stagnant = stagnant.saturating_add(1);
    } else {
        if current > *previous {
            *wraps = wraps.wrapping_add(1);
        }
        *previous = current;
        *stagnant = 0;
    }
    elapsed_ticks(start, *wraps) >= deadline || *stagnant >= POLL_FALLBACK_LIMIT
}

fn wait_ssts_det(hba: &Hba, p: u8) -> bool {
    if hba.pr32(p, port_off::SSTS) & SSTS_DET_MASK == SSTS_DET_ESTAB { return true; }
    if init_count() != 0 {
        let deadline = ms_to_ticks(100);
        let start = curr_count();
        let mut wraps = 0;
        let mut previous = start;
        let mut stagnant = 0;
        loop {
            if hba.pr32(p, port_off::SSTS) & SSTS_DET_MASK == SSTS_DET_ESTAB { return true; }
            if poll_timed_out(start, deadline, &mut wraps, &mut previous, &mut stagnant) { return false; }
            core::hint::spin_loop();
        }
    } else {
        for _ in 0..10_000_000 {
            if hba.pr32(p, port_off::SSTS) & SSTS_DET_MASK == SSTS_DET_ESTAB { return true; }
            core::hint::spin_loop();
        }
        false
    }
}

// ── Port state ──────────────────────────────────────────────────

struct PortPtr(*const AhciPort);
unsafe impl Send for PortPtr {}
unsafe impl Sync for PortPtr {}

static IRQ_PORTS: Mutex<Vec<PortPtr>> = Mutex::new(Vec::new());

struct AhciPort {
    root: u64,
    hba: Hba,
    port: u8,
    _cl_paddr: u64,
    cl_vaddr: u64,
    scratch_paddr: u64,
    scratch_vaddr: u64,
    max_prdt: usize,
    n_slots: u8,
    sector_count: u64,
    lba48: bool,
    ncq: bool,
    model: [u8; 40],
    slots: [Slot; AHCI_MAX_SLOTS],
    slot_alloc: core::sync::atomic::AtomicU32,
    irq_completed: AtomicU32,
    irq_vector: u8,
}

unsafe impl Sync for AhciPort {}

// ── Global AHCI interrupt handler ──────────────────────────────
//
// Called from the IDT device interrupt dispatch (irq_33..irq_48).
// Reads PxIS from all active ports, clears the status, and records
// completion in the per-port `irq_completed` mask.

fn handle_ahci_irq() {
    let ports = IRQ_PORTS.lock();
    for pptr in ports.iter() {
        let port = unsafe { &*pptr.0 };
        let is = port.hba.pr32(port.port, port_off::IS);
        if is != 0 {
            port.hba.pw32(port.port, port_off::IS, is);
            port.irq_completed.store(1, core::sync::atomic::Ordering::Release);
        }
    }
}

// ── Low-level helpers ───────────────────────────────────────────

impl AhciPort {
    /// Wait for one or more command slots to complete.
    /// Polls both PxCI and PxSACT until all mask bits are cleared.
    fn wait_slots(&self, tag_mask: u32) -> Result<(), &'static str> {
        let deadline = ms_to_ticks(5000);
        let start = curr_count();
        let mut wraps = 0;
        let mut previous = start;
        let mut stagnant = 0;
        // Clear any stale IRQ flag before waiting.
        self.irq_completed.store(0, core::sync::atomic::Ordering::Release);
        loop {
            // Fast path: IRQ fired — re-check hardware completion status.
            if self.irq_completed.load(core::sync::atomic::Ordering::Acquire) != 0 {
                self.irq_completed.store(0, core::sync::atomic::Ordering::Release);
                let ci = self.hba.pr32(self.port, port_off::CI);
                let sact = self.hba.pr32(self.port, port_off::SACT);
                if (ci & tag_mask) == 0 && (sact & tag_mask) == 0 {
                    let tfd = self.hba.pr32(self.port, port_off::TFD);
                    if tfd & TFD_ERR != 0 {
                        let serr = self.hba.pr32(self.port, port_off::SERR);
                        self.dump_err(tag_mask, (tfd >> 8) as u8, serr);
                        return Err("AHCI cmd error");
                    }
                    return Ok(());
                }
                continue;
            }
            // Polling fallback (also catches commands on non-IRQ paths).
            let ci = self.hba.pr32(self.port, port_off::CI);
            let sact = self.hba.pr32(self.port, port_off::SACT);
            if (ci & tag_mask) == 0 && (sact & tag_mask) == 0 {
                let tfd = self.hba.pr32(self.port, port_off::TFD);
                if tfd & TFD_ERR != 0 {
                    let serr = self.hba.pr32(self.port, port_off::SERR);
                    self.dump_err(tag_mask, (tfd >> 8) as u8, serr);
                    return Err("AHCI cmd error");
                }
                return Ok(());
            }
            if poll_timed_out(start, deadline, &mut wraps, &mut previous, &mut stagnant) {
                let tfd = self.hba.pr32(self.port, port_off::TFD);
                let serr = self.hba.pr32(self.port, port_off::SERR);
                self.dump_err(tag_mask, (tfd >> 8) as u8, serr);
                return Err("AHCI timeout");
            }
            core::hint::spin_loop();
        }
    }

    fn dump_err(&self, _tag_mask: u32, err: u8, serr: u32) {
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[ahci] ERR err=0x");
        SerialPort::put_hex(err as u64);
        if err & 0x04 != 0 { SerialPort::puts(" ABRT"); }
        if err & 0x10 != 0 { SerialPort::puts(" IDNF"); }
        if err & 0x40 != 0 { SerialPort::puts(" UNC"); }
        if err & 0x80 != 0 { SerialPort::puts(" WP"); }
        SerialPort::puts(" serr=0x");
        SerialPort::put_hex(serr as u64);
        SerialPort::puts("\n");
    }

    /// Write a Register H2D FIS for FPDMA QUEUED (NCQ).
    ///
    /// Per SATA 3.2 section 13.6.4.1 the NCQ frame re-maps several standard
    /// Register H2D FIS bytes (offsets from FIS base):
    ///   byte  3: sector_count_low  (feature 7:0)
    ///   byte  7: FUA (bit 7)      (device, bit 6 reserved)
    ///   byte 11: sector_count_high (feature 15:8)
    ///   byte 12: NCQ tag[4:0]<<3  (count 7:0)
    ///   byte 13: priority         (count 15:8)
    ///   bytes 16-19: auxiliary (0 for normal NCQ)
    ///   bytes 4-6, 8-10: LBA (standard 48-bit)
    fn write_ncq_fis(&self, fis: *mut u32, lba: u64, count: u16, tag: u8, cmd: u8) {
        unsafe {
            fis.add(0).write_volatile(0x8027u32 | (cmd as u32) << 16 | (count as u32) << 24);
            fis.add(1).write_volatile((lba as u32 & 0x00FF_FFFF) | (0x40 << 24));
            fis.add(2).write_volatile(
                ((lba >> 24) as u32 & 0xFF)
                | (((lba >> 32) as u32 & 0xFF) << 8)
                | (((lba >> 40) as u32 & 0xFF) << 16)
                | ((count as u32 >> 8) << 24));
            fis.add(3).write_volatile(((tag as u32) << 3) | (0 << 24));
            fis.add(4).write_volatile(0);
        }
    }

    /// Write a standard Register H2D FIS (non-NCQ).
    /// For 28-bit LBA commands (0xC8/0xCA): device reg includes LBA[27:24].
    /// For 48-bit LBA commands (0x25/0x35): LBA goes to bytes 4-6 and 8-10.
    fn write_std_fis(&self, fis: *mut u32, lba: u64, count: u16, cmd: u8) {
        unsafe {
            fis.add(0).write_volatile(0x8027u32 | (cmd as u32) << 16);
            if self.lba48 {
                fis.add(1).write_volatile((lba as u32 & 0x00FF_FFFF) | (0xE0 << 24));
                fis.add(2).write_volatile(
                    ((lba >> 24) as u32 & 0xFF)
                    | (((lba >> 32) as u32 & 0xFF) << 8)
                    | (((lba >> 40) as u32 & 0xFF) << 16));
                fis.add(3).write_volatile((count as u32 & 0xFF) | ((count as u32 >> 8) << 8));
            } else {
                let dev = 0xE0u32 | ((lba >> 24) as u32 & 0x0F);
                fis.add(1).write_volatile((lba as u32 & 0x00FF_FFFF) | (dev << 24));
                fis.add(2).write_volatile(0);
                fis.add(3).write_volatile(count as u32 & 0xFF);
            }
            fis.add(4).write_volatile(0);
        }
    }

    /// Build PRDT entries with translation caching.
    fn build_prdt(&self, buf_vaddr: u64, size: usize, prdt_ptr: *mut PrdEntry) -> Result<usize, &'static str> {
        let mut rem = size as isize;
        let mut off: isize = 0;
        let mut n = 0usize;
        while rem > 0 && n < self.max_prdt {
            let va = (buf_vaddr as isize + off) as u64;
            let pa = dma::translate(self.root, va).ok_or("PRDT translate fail")?;
            let skip = (va & 0xFFF) as usize;
            let chunk = rem.min((4096 - skip) as isize) as usize;
            unsafe {
                let e = prdt_ptr.add(n);
                let dba = pa | (skip as u64);
                (*e).dba = dba as u32;
                (*e).dbau = (dba >> 32) as u32;
                (*e)._rsvd = 0;
                (*e).dbc = (chunk - 1) as u32;
            }
            n += 1;
            rem -= chunk as isize;
            off += chunk as isize;
        }
        if rem > 0 { return Err("PRDT entries exhausted"); }
        Ok(n)
    }

    /// Allocate `count` NCQ command slots. Returns bitmask of allocated tags.
    fn alloc_slots(&self, count: usize) -> Result<u32, &'static str> {
        loop {
            let current = self.slot_alloc.load(core::sync::atomic::Ordering::Relaxed);
            let mask = if self.n_slots >= 32 { !0u32 } else { (1u32 << self.n_slots) - 1 };
            let free = !current & mask;
            if (free.count_ones() as usize) < count {
                return Err("not enough NCQ slots");
            }
            let mut mask = 0u32;
            let mut found = 0usize;
            for tag in 0..self.n_slots {
                if free & (1 << tag) != 0 && found < count {
                    mask |= 1 << tag;
                    found += 1;
                }
            }
            if self.slot_alloc.compare_exchange_weak(
                current, current | mask,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Relaxed,
            ).is_ok() {
                return Ok(mask);
            }
        }
    }

    fn free_slots(&self, tag_mask: u32) {
        self.slot_alloc.fetch_and(!tag_mask, core::sync::atomic::Ordering::Release);
    }

    /// Prepare a command in the given slot.
    fn prepare_cmd(&self, tag: u8, lba: u64, count: u32, buf_vaddr: u64, size: usize, is_write: bool) -> Result<(), &'static str> {
        let ct_va = self.slots[tag as usize].ct_vaddr;

        unsafe {
            core::ptr::write_bytes(ct_va as *mut u8, 0, 0x80 + self.max_prdt * 16);
        }

        if self.ncq {
            let cmd = if is_write { 0x61u8 } else { 0x60u8 };
            self.write_ncq_fis(ct_va as *mut u32, lba, count as u16, tag, cmd);
        } else if self.lba48 {
            let cmd = if is_write { 0x35u8 } else { 0x25u8 };
            self.write_std_fis(ct_va as *mut u32, lba, count as u16, cmd);
        } else {
            let cmd = if is_write { 0xCAu8 } else { 0xC8u8 };
            self.write_std_fis(ct_va as *mut u32, lba, count as u16, cmd);
        }

        let prdt_ptr = (ct_va + 0x80) as *mut PrdEntry;
        let prdtl = if buf_vaddr != 0 {
            self.build_prdt(buf_vaddr, size, prdt_ptr)?
        } else {
            unsafe {
                (*prdt_ptr).dba = self.scratch_paddr as u32;
                (*prdt_ptr).dbau = (self.scratch_paddr >> 32) as u32;
                (*prdt_ptr)._rsvd = 0;
                (*prdt_ptr).dbc = (size - 1) as u32;
            }
            1usize
        };

        let hdr = (self.cl_vaddr + (tag as u64) * 32) as *mut CmdHeader;
        let w = if is_write { 1u32 << 7 } else { 0 };
        unsafe {
            (*hdr).cfl_w_prdtl = 5u32 | w | (prdtl as u32) << 16;
            (*hdr).prdbc = 0;
        }

        Ok(())
    }

    /// Submit a batch of commands identified by tag_mask.
    /// For NCQ: writes PxSACT then PxCI, waits for completion.
    /// For non-NCQ: writes only PxCI, waits for completion.
    fn submit_batch(&self, tag_mask: u32) -> Result<(), &'static str> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        if self.ncq {
            self.hba.pw32(self.port, port_off::SACT, tag_mask);
        }
        self.hba.pw32(self.port, port_off::CI, tag_mask);
        self.wait_slots(tag_mask)
    }

    // ── Port reset ─────────────────────────────────────────────

    fn port_reset(&self) -> Result<(), &'static str> {
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[ahci] port reset\n");

        // Stop port DMA
        let cmd = self.hba.pr32(self.port, port_off::CMD);
        self.hba.pw32(self.port, port_off::CMD, cmd & !(CMD_ST | CMD_FRE));
        for _ in 0..1000 {
            let c = self.hba.pr32(self.port, port_off::CMD);
            if c & (CMD_CR | CMD_FR) == 0 { break; }
            core::hint::spin_loop();
        }

        // Save port registers that may be reset by COMRESET on some hardware.
        let saved_clb  = self.hba.pr32(self.port, port_off::CLB);
        let saved_clbu = self.hba.pr32(self.port, port_off::CLBU);
        let saved_fb   = self.hba.pr32(self.port, port_off::FB);
        let saved_fbu  = self.hba.pr32(self.port, port_off::FBU);
        let saved_ie   = self.hba.pr32(self.port, port_off::IE);

        // Issue COMRESET via SCTL.DET = 1
        let sctl = self.hba.pr32(self.port, port_off::SCTL);
        self.hba.pw32(self.port, port_off::SCTL, (sctl & !0x0F) | 1);
        let start = curr_count();
        let mut wraps = 0;
        let mut previous = start;
        let mut stagnant = 0;
        while !poll_timed_out(start, ms_to_ticks(2), &mut wraps, &mut previous, &mut stagnant) {
            core::hint::spin_loop();
        }
        self.hba.pw32(self.port, port_off::SCTL, sctl & !0x0F);

        // Wait up to 100ms for device to re-establish
        let start = curr_count();
        let mut wraps = 0;
        let mut previous = start;
        let mut stagnant = 0;
        loop {
            if self.hba.pr32(self.port, port_off::SSTS) & SSTS_DET_MASK == SSTS_DET_ESTAB {
                break;
            }
            if poll_timed_out(start, ms_to_ticks(100), &mut wraps, &mut previous, &mut stagnant) {
                SerialPort::puts("[ahci] port reset timeout\n");
                return Err("port reset timeout");
            }
            core::hint::spin_loop();
        }

        // Restore port registers that may have been cleared by COMRESET.
        self.hba.pw32(self.port, port_off::CLB, saved_clb);
        self.hba.pw32(self.port, port_off::CLBU, saved_clbu);
        self.hba.pw32(self.port, port_off::FB, saved_fb);
        self.hba.pw32(self.port, port_off::FBU, saved_fbu);

        self.hba.pw32(self.port, port_off::SERR, !0);
        self.hba.pw32(self.port, port_off::IS, !0);
        self.hba.pw32(self.port, port_off::CMD, CMD_FRE | CMD_ST);

        // Re-enable interrupts if they were previously enabled.
        if saved_ie != 0 {
            self.hba.pw32(self.port, port_off::IE, saved_ie);
        }

        SerialPort::puts("[ahci] port reset OK\n");
        Ok(())
    }

    // ── IDENTIFY ───────────────────────────────────────────────

    fn identify(&mut self) -> Result<(), &'static str> {
        let ct_va = self.slots[0].ct_vaddr;

        // Zero FIS + PRDT area
        unsafe { core::ptr::write_bytes(ct_va as *mut u8, 0, 0x80 + self.max_prdt * 16); }

        // IDENTIFY DEVICE command (non-NCQ, uses same Register H2D FIS format)
        self.write_ncq_fis(ct_va as *mut u32, 0, 0u16, 0, 0xEC);

        // PRDT -> scratch buffer
        let prdt_ptr = (ct_va + 0x80) as *mut PrdEntry;
        unsafe {
            (*prdt_ptr).dba = self.scratch_paddr as u32;
            (*prdt_ptr).dbau = (self.scratch_paddr >> 32) as u32;
            (*prdt_ptr)._rsvd = 0;
            (*prdt_ptr).dbc = 511;
        }

        // CmdHeader slot 0
        let hdr = self.cl_vaddr as *mut CmdHeader;
        unsafe {
            (*hdr).cfl_w_prdtl = 5u32 | (1u32 << 16);
            (*hdr).prdbc = 0;
        }

        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        self.hba.pw32(self.port, port_off::CI, 1);
        self.wait_slots(1)?;

        unsafe {
            let data = self.scratch_vaddr as *const u16;
            let w83 = data.add(83).read_volatile();
            self.lba48 = (w83 & (1 << 10)) != 0;

            if self.lba48 {
                let w100 = data.add(100).read_volatile() as u64;
                let w101 = data.add(101).read_volatile() as u64;
                let w102 = data.add(102).read_volatile() as u64;
                let w103 = data.add(103).read_volatile() as u64;
                self.sector_count = w100 | (w101 << 16) | (w102 << 32) | (w103 << 48);
            } else {
                let w60 = data.add(60).read_volatile() as u64;
                let w61 = data.add(61).read_volatile() as u64;
                self.sector_count = w60 | (w61 << 16);
            }

            if self.sector_count == 0 {
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[ahci] WARN: IDENTIFY sector_count=0\n");
            }

            // Check NCQ support (IDENTIFY word 76, bit 8)
            let w76 = data.add(76).read_volatile();
            self.ncq = (w76 & (1 << 8)) != 0;

            for i in 0..20 {
                let w = data.add(27 + i).read_volatile();
                self.model[i * 2] = (w >> 8) as u8;
                self.model[i * 2 + 1] = (w & 0xFF) as u8;
            }
            for b in self.model.iter_mut().rev() {
                if *b == b' ' || *b == 0 { *b = 0; } else { break; }
            }
        }

        Ok(())
    }
}

// ── BlockDevice trait ───────────────────────────────────────────

impl BlockDevice for AhciPort {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str> {
        let n = reqs.len().min(if self.ncq { self.n_slots as usize } else { 1 });
        if n == 0 {
            return Ok(IoCompletions { completed: 0, errors: 0 });
        }

        // Allocate tags (NCQ or non-NCQ)
        let tag_mask = self.alloc_slots(n)?;

        // Phase 1: prepare all command slots
        let mut tag_of = [0u8; AHCI_MAX_SLOTS];
        let mut idx = 0usize;
        let mut err_mask = 0u32;
        for tag in 0..self.n_slots as u8 {
            if tag_mask & (1 << tag) != 0 {
                if idx < n {
                    tag_of[idx] = tag;
                    let req = &reqs[idx];
                    let bytes = (req.count as usize) * 512;
                    let (buf_vaddr, buf_size) = match &req.buffer {
                        IoBuffer::Buf(buf) => (buf.as_ptr() as u64, buf.len()),
                        IoBuffer::ConstBuf(buf) => (buf.as_ptr() as u64, buf.len()),
                        IoBuffer::Phys(pa, sz) => (*pa, *sz),
                    };
                    if buf_size < bytes {
                        err_mask |= 1 << tag;
                    } else if let Err(_) = self.prepare_cmd(tag, req.lba, req.count, buf_vaddr, bytes, req.is_write) {
                        err_mask |= 1 << tag;
                    }
                    idx += 1;
                }
            }
        }

        // Phase 2: submit batch (only successful tags)
        let ok_mask = tag_mask & !err_mask;
        if ok_mask == 0 {
            self.free_slots(tag_mask);
            return Ok(IoCompletions { completed: 0, errors: tag_mask });
        }

        let result = self.submit_batch(ok_mask);
        match result {
            Ok(()) => {
                self.free_slots(tag_mask);
                Ok(IoCompletions { completed: ok_mask, errors: err_mask })
            }
            Err(_e) => {
                // Port reset + retry once
                if self.port_reset().is_err() {
                    self.free_slots(tag_mask);
                    return Err("AHCI port reset failed");
                }
                // Re-prepare
                for i in 0..n {
                    let tag = tag_of[i];
                    if err_mask & (1 << tag) != 0 { continue; }
                    let req = &reqs[i];
                    let bytes = (req.count as usize) * 512;
                    let (buf_vaddr, buf_size) = match &req.buffer {
                        IoBuffer::Buf(buf) => (buf.as_ptr() as u64, buf.len()),
                        IoBuffer::ConstBuf(buf) => (buf.as_ptr() as u64, buf.len()),
                        IoBuffer::Phys(pa, sz) => (*pa, *sz),
                    };
                    if buf_size >= bytes {
                        let _ = self.prepare_cmd(tag, req.lba, req.count, buf_vaddr, bytes, req.is_write);
                    }
                }
                match self.submit_batch(ok_mask) {
                    Ok(()) => {
                        self.free_slots(tag_mask);
                        Ok(IoCompletions { completed: ok_mask, errors: err_mask })
                    }
                    Err(e2) => {
                        self.free_slots(tag_mask);
                        Err(e2)
                    }
                }
            }
        }
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn model_string(&self) -> &str {
        let t = core::str::from_utf8(&self.model).unwrap_or("(bad utf8)");
        t.trim_end_matches(char::from(0))
    }
}

// ── Initialisation ──────────────────────────────────────────────

fn init_controller(dev: &crate::pci::PciDevice, dma: &mut DmaAllocator) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str> {
    use crate::drivers::serial::SerialPort;
    let base = match crate::pci::bar::bar(dev, 5) {
        crate::pci::bar::Bar::Memory { addr, .. } => addr,
        _ => {
            SerialPort::puts("[ahci] BAR5 is not memory-mapped\n");
            return Ok(Vec::new());
        }
    };

    let mmio = Hba { vaddr: dma.map_mmio(base, 0x1000)? };
    let cap = mmio.r32(ghc::CAP);
    let n_ports = ((cap >> 17) & 0x1F) + 1;
    let pi = mmio.r32(ghc::PI);
    let n_slots_raw = ((cap >> 8) & 0x1F) + 1;
    let max_prdt_raw = (4096 - 0x80) / 16;

    let mmio_sz = (((0x100 + (n_ports as u64) * 0x80 + 0x80) + 0xFFF) & !0xFFF).max(0x1000);
    let mmio = Hba { vaddr: dma.map_mmio(base, mmio_sz)? };

    let ver = mmio.r32(ghc::VS);
    SerialPort::puts("[ahci] controller ");
    SerialPort::put_u64(dev.bus as u64);
    SerialPort::puts(":");
    SerialPort::put_u64(dev.device as u64);
    SerialPort::puts(":");
    SerialPort::put_u64(dev.function as u64);
    SerialPort::puts(" v");
    SerialPort::put_u64(((ver >> 16) & 0xFF) as u64);
    SerialPort::puts(".");
    SerialPort::put_u64(((ver >> 8) & 0xFF) as u64);
    SerialPort::puts(".");
    SerialPort::put_u64((ver & 0xFF) as u64);
    SerialPort::puts(" ports=");
    SerialPort::put_u64(n_ports as u64);
    SerialPort::puts(" pi=0x");
    SerialPort::put_hex(pi as u64);
    SerialPort::puts(" slots=");
    SerialPort::put_u64(n_slots_raw as u64);
    SerialPort::puts("\n");

    // HBA reset. Keep this phase visible: an HBA that never acknowledges a
    // reset cannot be safely probed.
    SerialPort::puts("[ahci] resetting HBA\n");
    mmio.w32(ghc::GHC, GHC_HR);
    for _ in 0..1000 {
        if mmio.r32(ghc::GHC) & GHC_HR == 0 { break; }
        core::hint::spin_loop();
    }
    if mmio.r32(ghc::GHC) & GHC_HR != 0 {
        SerialPort::puts("[ahci] HBA reset timeout\n"); return Ok(Vec::new());
    }
    mmio.w32(ghc::GHC, mmio.r32(ghc::GHC) | GHC_AE);
    SerialPort::puts("[ahci] HBA reset complete\n");

    let mut ports: Vec<Arc<dyn BlockDevice>> = Vec::new();
    for p in 0u8..n_ports.min(32) as u8 {
        if pi & (1 << p) == 0 { continue; }

        match init_one(p, &mmio, dma, max_prdt_raw.min(MAX_PRDT), n_slots_raw, dev.interrupt_line) {
            Ok(port) => {
                let port_arc = Arc::new(port);
                let ptr = Arc::as_ptr(&port_arc);
                IRQ_PORTS.lock().push(PortPtr(ptr));
                ports.push(port_arc as Arc<dyn BlockDevice>);
                SerialPort::puts("[ahci] port ");
                SerialPort::put_u64(p as u64);
                SerialPort::puts(" ready (");
                SerialPort::put_u64(ports.len() as u64);
                SerialPort::puts("/");
                SerialPort::put_u64(n_ports as u64);
                SerialPort::puts(")\n");
            }
            Err(e) => {
                SerialPort::puts("[ahci] port ");
                SerialPort::put_u64(p as u64);
                SerialPort::puts(" fail: ");
                SerialPort::puts(e);
                SerialPort::puts("\n");
            }
        }
    }
    Ok(ports)
}
fn init_one(p: u8, hba: &Hba, dma: &mut DmaAllocator, max_prdt: usize, n_slots_raw: u32, interrupt_line: u8) -> Result<AhciPort, &'static str> {
    let n_slots = (n_slots_raw as usize).min(AHCI_MAX_SLOTS) as u8;

    // Stop port DMA
    let cmd = hba.pr32(p, port_off::CMD);
    hba.pw32(p, port_off::CMD, cmd & !(CMD_ST | CMD_FRE));
    for _ in 0..1000 {
        if hba.pr32(p, port_off::CMD) & (CMD_CR | CMD_FR) == 0 { break; }
        core::hint::spin_loop();
    }

    // Allocate Command List (1 page = 32 CmdHeaders × 32 B = 1024 B)
    let cl_buf = dma.alloc_page().ok_or("OOM CL")?;

    // Allocate scratch buffer
    let sc_buf = dma.alloc_page().ok_or("OOM scratch")?;

    // Allocate per-slot Command Table pages
    let mut slots = [Slot { ct_paddr: 0, ct_vaddr: 0 }; AHCI_MAX_SLOTS];
    for s in 0..n_slots as usize {
        let ct_buf = dma.alloc_page().ok_or("OOM CT")?;
        slots[s] = Slot { ct_paddr: ct_buf.phys, ct_vaddr: ct_buf.virt };
    }

    // Pre-initialise CmdHeaders for all slots
    for s in 0..n_slots as usize {
        let hdr = (cl_buf.virt + (s as u64) * 32) as *mut CmdHeader;
        let ctba = slots[s].ct_paddr;
        unsafe {
            (*hdr).cfl_w_prdtl = 5u32;  // CFL=5, W=0, PRDTL=0
            (*hdr).prdbc = 0;
            (*hdr).ctba = ctba as u32;
            (*hdr).ctbau = (ctba >> 32) as u32;
            core::ptr::write_bytes((hdr as *mut u32).add(4) as *mut u8, 0, 16);
        }
    }

    // Program HBA port registers
    hba.pw32(p, port_off::CLB, cl_buf.phys as u32);
    hba.pw32(p, port_off::CLBU, (cl_buf.phys >> 32) as u32);
    hba.pw32(p, port_off::FB, sc_buf.phys as u32);
    hba.pw32(p, port_off::FBU, (sc_buf.phys >> 32) as u32);

    hba.pw32(p, port_off::IS, !0);
    hba.pw32(p, port_off::SERR, !0);
    hba.pw32(p, port_off::IE, 0);

    // Attempt IRQ setup: register handler, enable IOAPIC, enable port interrupts.
    let mut irq_vector = 0u8;
    if interrupt_line != 0 {
        if let Some(vector) = crate::arch::x86_64::idt::register_device_handler(handle_ahci_irq) {
            let _ = crate::platform::x86_64_pc::ioapic::enable_irq(
                interrupt_line as u32,
                crate::acpi::Polarity::ActiveHigh,
                crate::acpi::TriggerMode::Edge,
            );
            irq_vector = vector;
        }
    }

    hba.pw32(p, port_off::CMD, hba.pr32(p, port_off::CMD) | CMD_SUD | CMD_POD);
    for _ in 0..100 { core::hint::spin_loop(); }

    hba.pw32(p, port_off::CMD, hba.pr32(p, port_off::CMD) | CMD_FRE);
    hba.pw32(p, port_off::CMD, hba.pr32(p, port_off::CMD) | CMD_ST);

    if !wait_ssts_det(hba, p) { return Err("no device"); }

    let mut port = AhciPort {
        root: dma.root(),
        hba: *hba, port: p,
        _cl_paddr: cl_buf.phys, cl_vaddr: cl_buf.virt,
        scratch_paddr: sc_buf.phys, scratch_vaddr: sc_buf.virt,
        max_prdt, n_slots,
        sector_count: 0, lba48: false, ncq: false, model: [0u8; 40],
        slots,
        slot_alloc: core::sync::atomic::AtomicU32::new(0),
        irq_completed: AtomicU32::new(0),
        irq_vector,
    };

    port.identify()?;

    if irq_vector != 0 {
        hba.pw32(p, port_off::IE, 0x0000_0089);
    }

    Ok(port)
}

pub struct AhciDriver;

impl StorageDriver for AhciDriver {
    fn name(&self) -> &str {
        "ahci"
    }

    fn probe(&self, dev: &crate::pci::PciDevice) -> bool {
        dev.class == 0x01 && dev.subclass == 0x06 && dev.prog_if == 0x01
    }

    fn init_controller(
        &self,
        dev: &crate::pci::PciDevice,
        dma: &mut DmaAllocator,
    ) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str> {
        init_controller(dev, dma)
    }
}
