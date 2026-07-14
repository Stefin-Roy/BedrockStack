//! AHCI (Advanced Host Controller Interface) SATA driver.
//!
//! Polling-mode driver for the Q35 ICH9 AHCI controller.
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

use core::ptr::{read_volatile, write_volatile};
use alloc::sync::Arc;
use spin::Mutex;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{Vmm, PageFlags, KERNEL_VMA_BASE};
use crate::platform::x86_64_pc::apic;
use super::traits::{BlockDevice, IoRequest, IoBuffer, IoCompletions};

const AHCI_MAX_SLOTS: usize = 32;
const MAX_PRDT: usize = 64;
const TRANS_CACHE_SIZE: usize = 64;

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

const TFD_ERR: u32 = 1 << 8;

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

// ── Translation cache ──────────────────────────────────────────
//
// Avoids repeated 4-level page walks when the same physical page
// is referenced across multiple PRDT entries or commands.

struct TransCacheInner {
    entries: [(u64, u64); TRANS_CACHE_SIZE],
    next: usize,
}

struct TransCache {
    data: core::cell::UnsafeCell<TransCacheInner>,
}

unsafe impl Sync for TransCache {}

impl TransCache {
    const fn new() -> Self {
        TransCache {
            data: core::cell::UnsafeCell::new(TransCacheInner {
                entries: [(0, 0); TRANS_CACHE_SIZE],
                next: 0,
            }),
        }
    }

    fn lookup_or_translate(&self, vaddr: u64) -> Option<u64> {
        let inner = unsafe { &mut *self.data.get() };
        let vaddr_page = vaddr & !0xFFF;
        for &(v, p) in &inner.entries {
            if v == vaddr_page {
                return Some(p);
            }
        }
        let pa = translate(vaddr_page)?;
        let idx = inner.next % TRANS_CACHE_SIZE;
        inner.entries[idx] = (vaddr_page, pa);
        inner.next = inner.next.wrapping_add(1);
        Some(pa)
    }
}

static TRANS_CACHE: TransCache = TransCache::new();

// ── VMM for MMIO mapping & address translation ──────────────────

static VMM_STATE: Mutex<Option<VmmState>> = Mutex::new(None);

struct VmmState {
    root: u64,
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
}

unsafe impl Send for VmmState {}
unsafe impl Sync for VmmState {}

fn map_mmio(paddr: u64, size: u64) -> u64 {
    let mut g = VMM_STATE.lock();
    let s = g.as_mut().expect("VMM not init");
    let va = s.next_vaddr.checked_sub(size).expect("AHCI VMM: address space exhausted (overflow)");
    if va < VMM_VADDR_FLOOR {
        panic!("AHCI VMM: address space exhausted (vaddr {:#x} would overlap adjacent region)", va);
    }
    s.next_vaddr = va;
    Vmm::from_root(s.root)
        .map(unsafe { &mut *s.alloc }, va, paddr, size,
             PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE);
    va
}

fn translate(vaddr: u64) -> Option<u64> {
    let g = VMM_STATE.lock();
    let s = g.as_ref()?;
    Vmm::from_root(s.root).translate(vaddr)
}

// ── APIC timer helpers ──────────────────────────────────────────

fn init_count() -> u32 { apic::timer_init_count() }
fn curr_count() -> u32 { apic::timer_current_count() }

fn elapsed_ticks(start: u32) -> u32 {
    let i = init_count();
    let c = curr_count();
    if c <= start { start - c } else { start + (i - c) }
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

// ── Port state ──────────────────────────────────────────────────

static PORT: Mutex<Option<Arc<AhciPort>>> = Mutex::new(None);

struct AhciPort {
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
    model: [u8; 40],
    slots: [Slot; AHCI_MAX_SLOTS],
    slot_alloc: core::sync::atomic::AtomicU32,
}

unsafe impl Sync for AhciPort {}

// ── Low-level helpers ───────────────────────────────────────────

impl AhciPort {
    /// Wait for one or more command slots to complete.
    /// Polls both PxCI and PxSACT until all mask bits are cleared.
    fn wait_slots(&self, tag_mask: u32) -> Result<(), &'static str> {
        let deadline = ms_to_ticks(5000);
        let start = curr_count();
        loop {
            let ci = self.hba.pr32(self.port, port_off::CI);
            let sact = self.hba.pr32(self.port, port_off::SACT);
            if (ci & tag_mask) == 0 && (sact & tag_mask) == 0 {
                let tfd = self.hba.pr32(self.port, port_off::TFD);
                if tfd & TFD_ERR != 0 {
                    let serr = self.hba.pr32(self.port, port_off::SERR);
                    self.dump_err(tag_mask, tfd as u8, serr);
                    return Err("AHCI cmd error");
                }
                return Ok(());
            }
            if elapsed_ticks(start) >= deadline {
                let tfd = self.hba.pr32(self.port, port_off::TFD);
                let serr = self.hba.pr32(self.port, port_off::SERR);
                self.dump_err(tag_mask, tfd as u8, serr);
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

    /// Write a Register H2D FIS for FPDMA QUEUED (NCQ) using u32 stores.
    ///
    /// Layout (little-endian u32):
    ///   dword0: type=0x27 | 0x80<<8 | cmd<<16
    ///   dword1: LBA[23:0] | device(0x40)<<24
    ///   dword2: LBA[31:24] | LBA[39:32]<<8 | count<<16
    ///   dword3: tag<<11
    ///   dword4: 0 (reserved)
    fn write_ncq_fis(&self, fis: *mut u32, lba: u64, count: u32, cmd: u8, tag: u8) {
        unsafe {
            fis.add(0).write_volatile(0x8027u32 | (cmd as u32) << 16);
            fis.add(1).write_volatile((lba as u32 & 0x00FF_FFFF) | (0x40 << 24));
            fis.add(2).write_volatile(
                ((lba >> 24) as u32 & 0xFF)
                | (((lba >> 32) as u32 & 0xFF) << 8)
                | (count << 16));
            fis.add(3).write_volatile((tag as u32) << 11);
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
            let pa = TRANS_CACHE.lookup_or_translate(va).ok_or("PRDT translate fail")?;
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
            let free = !current & ((1u32 << self.n_slots) - 1);
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

    /// Prepare a single NCQ command in the given slot.
    fn prepare_cmd(&self, tag: u8, lba: u64, count: u32, buf_vaddr: u64, size: usize, is_write: bool) -> Result<(), &'static str> {
        let ct_va = self.slots[tag as usize].ct_vaddr;

        // Zero only FIS + PRDT area (not entire 4K page)
        unsafe {
            core::ptr::write_bytes(ct_va as *mut u8, 0, 0x80 + self.max_prdt * 16);
        }

        // Write NCQ FIS
        let cmd = if is_write { 0x61u8 } else { 0x60u8 };
        self.write_ncq_fis(ct_va as *mut u32, lba, count, cmd, tag);

        // Build PRDT
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

        // Update CmdHeader (W bit, PRDTL, clear PRDBC)
        let hdr = (self.cl_vaddr + (tag as u64) * 32) as *mut CmdHeader;
        let w = if is_write { 1u32 << 7 } else { 0 };
        unsafe {
            (*hdr).cfl_w_prdtl = 5u32 | w | (prdtl as u32) << 16;
            (*hdr).prdbc = 0;
        }

        Ok(())
    }

    /// Submit a batch of NCQ commands identified by tag_mask.
    /// Writes PxSACT then PxCI, waits for completion.
    fn submit_batch(&self, tag_mask: u32) -> Result<(), &'static str> {
        self.hba.pw32(self.port, port_off::SACT, tag_mask);
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        self.hba.pw32(self.port, port_off::CI, tag_mask);
        self.wait_slots(tag_mask)
    }

    // ── Port reset ─────────────────────────────────────────────

    fn port_reset(&self) -> Result<(), &'static str> {
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[ahci] port reset\n");

        let cmd = self.hba.pr32(self.port, port_off::CMD);
        self.hba.pw32(self.port, port_off::CMD, cmd & !(CMD_ST | CMD_FRE));
        for _ in 0..1000 {
            let c = self.hba.pr32(self.port, port_off::CMD);
            if c & (CMD_CR | CMD_FR) == 0 { break; }
            core::hint::spin_loop();
        }

        let sctl = self.hba.pr32(self.port, port_off::SCTL);
        self.hba.pw32(self.port, port_off::SCTL, (sctl & !0x0F) | 1);
        let start = curr_count();
        while ticks_to_ms(elapsed_ticks(start)) < 2 { core::hint::spin_loop(); }
        self.hba.pw32(self.port, port_off::SCTL, sctl & !0x0F);

        let start = curr_count();
        loop {
            if self.hba.pr32(self.port, port_off::SSTS) & SSTS_DET_MASK == SSTS_DET_ESTAB {
                break;
            }
            if ticks_to_ms(elapsed_ticks(start)) >= 100 {
                SerialPort::puts("[ahci] port reset timeout\n");
                return Err("port reset timeout");
            }
            core::hint::spin_loop();
        }

        self.hba.pw32(self.port, port_off::SERR, !0);
        self.hba.pw32(self.port, port_off::IS, !0);
        self.hba.pw32(self.port, port_off::CMD, CMD_FRE | CMD_ST);

        SerialPort::puts("[ahci] port reset OK\n");
        Ok(())
    }

    // ── IDENTIFY ───────────────────────────────────────────────

    fn identify(&mut self) -> Result<(), &'static str> {
        let ct_va = self.slots[0].ct_vaddr;

        // Zero FIS + PRDT area
        unsafe { core::ptr::write_bytes(ct_va as *mut u8, 0, 0x80 + self.max_prdt * 16); }

        // IDENTIFY DEVICE command (non-NCQ, but write_ncq_fis works with tag=0, count=0)
        self.write_ncq_fis(ct_va as *mut u32, 0, 0, 0xEC, 0);

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
        let n = reqs.len().min(self.n_slots as usize);
        if n == 0 {
            return Ok(IoCompletions { completed: 0, errors: 0 });
        }

        // Allocate NCQ tags
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

const VMM_VADDR: u64 = KERNEL_VMA_BASE - 0x10000000 - 0x20000000 - 0x20000000;
/// AHCI VMM floor — 512 MB of virtual space for AHCI MMIO.
const VMM_VADDR_FLOOR: u64 = VMM_VADDR - 0x2000_0000;

pub fn init(root: u64, alloc: *mut BitmapAllocator) {
    use crate::drivers::serial::SerialPort;
    SerialPort::puts("[ahci] init\n");

    *VMM_STATE.lock() = Some(VmmState { root, alloc, next_vaddr: VMM_VADDR });

    let dev = {
        let mut d = None;
        for dv in crate::pci::devices() {
            if dv.class == 0x01 && dv.subclass == 0x06 && dv.prog_if == 0x01 { d = Some(*dv); break; }
        }
        d
    };

    let dev = match dev {
        Some(d) => d,
        None => { SerialPort::puts("[ahci] no AHCI controller\n"); return; }
    };

    let (base, ok) = bar5_addr(dev.bars);
    if !ok {
        SerialPort::puts("[ahci] invalid BAR5\n");
        return;
    }

    let mmio = Hba { vaddr: map_mmio(base, 0x1000) };
    let cap = mmio.r32(ghc::CAP);
    let n_ports = ((cap >> 17) & 0x1F) + 1;
    let pi = mmio.r32(ghc::PI);
    let n_slots_raw = ((cap >> 8) & 0x1F) + 1;
    let max_prdt_raw = (4096 - 0x80) / 16;

    let mmio_sz = (((0x100 + (n_ports as u64) * 0x80 + 0x80) + 0xFFF) & !0xFFF).max(0x1000);
    let mmio = Hba { vaddr: map_mmio(base, mmio_sz) };

    let ver = mmio.r32(ghc::VS);
    SerialPort::puts("[ahci] v");
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

    // HBA reset
    mmio.w32(ghc::GHC, GHC_HR);
    for _ in 0..1000 {
        if mmio.r32(ghc::GHC) & GHC_HR == 0 { break; }
        core::hint::spin_loop();
    }
    if mmio.r32(ghc::GHC) & GHC_HR != 0 {
        SerialPort::puts("[ahci] HBA reset timeout\n"); return;
    }
    mmio.w32(ghc::GHC, mmio.r32(ghc::GHC) | GHC_AE);

    // Probe
    for p in 0u8..n_ports.min(32) as u8 {
        if pi & (1 << p) == 0 { continue; }
        let ssts = mmio.pr32(p, port_off::SSTS);
        if ssts & SSTS_DET_MASK != SSTS_DET_ESTAB { continue; }
        if (ssts >> 8) & 0x0F != 1 { continue; }
        if mmio.pr32(p, port_off::SIG) != 0x0000_0101 { continue; }

        match init_one(p, &mmio, alloc, max_prdt_raw.min(MAX_PRDT), n_slots_raw) {
            Ok(port) => {
                *PORT.lock() = Some(Arc::new(port));
                SerialPort::puts("[ahci] port ");
                SerialPort::put_u64(p as u64);
                SerialPort::puts(" ready\n");
                return;
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
}

fn bar5_addr(bars: [u32; 6]) -> (u64, bool) {
    if bars[5] & 1 != 0 { return (0, false); }
    match bars[5] & 0x06 {
        0 => ((bars[5] & 0xFFFF_FFF0) as u64, true),
        4 => {
            let base = ((bars[5] as u64) & 0xFFFF_FFF0) | ((bars[4] as u64) << 32);
            (base, true)
        }
        _ => (0, false),
    }
}

fn init_one(p: u8, hba: &Hba, alloc: *mut BitmapAllocator, max_prdt: usize, n_slots_raw: u32) -> Result<AhciPort, &'static str> {
    let alloc = unsafe { &mut *alloc };
    let n_slots = (n_slots_raw as usize).min(AHCI_MAX_SLOTS) as u8;

    // Stop port DMA
    let cmd = hba.pr32(p, port_off::CMD);
    hba.pw32(p, port_off::CMD, cmd & !(CMD_ST | CMD_FRE));
    for _ in 0..1000 {
        if hba.pr32(p, port_off::CMD) & (CMD_CR | CMD_FR) == 0 { break; }
        core::hint::spin_loop();
    }

    // Allocate Command List (1 page = 32 CmdHeaders × 32 B = 1024 B)
    let cl_paddr = alloc.alloc().ok_or("OOM CL")?;
    unsafe { core::ptr::write_bytes(cl_paddr as *mut u8, 0, 4096); }

    // Allocate scratch buffer
    let sc_paddr = alloc.alloc().ok_or("OOM scratch")?;
    unsafe { core::ptr::write_bytes(sc_paddr as *mut u8, 0, 4096); }

    // Allocate per-slot Command Table pages
    let mut slots = [Slot { ct_paddr: 0, ct_vaddr: 0 }; AHCI_MAX_SLOTS];
    for s in 0..n_slots as usize {
        let ct_paddr = alloc.alloc().ok_or("OOM CT")?;
        unsafe { core::ptr::write_bytes(ct_paddr as *mut u8, 0, 4096); }
        slots[s] = Slot { ct_paddr, ct_vaddr: ct_paddr };
    }

    // Pre-initialise CmdHeaders for all slots
    for s in 0..n_slots as usize {
        let hdr = (cl_paddr + (s as u64) * 32) as *mut CmdHeader;
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
    hba.pw32(p, port_off::CLB, cl_paddr as u32);
    hba.pw32(p, port_off::CLBU, (cl_paddr >> 32) as u32);
    hba.pw32(p, port_off::FB, sc_paddr as u32);
    hba.pw32(p, port_off::FBU, (sc_paddr >> 32) as u32);

    hba.pw32(p, port_off::IS, !0);
    hba.pw32(p, port_off::SERR, !0);
    hba.pw32(p, port_off::IE, 0);

    hba.pw32(p, port_off::CMD, CMD_SUD | CMD_POD);
    for _ in 0..100 { core::hint::spin_loop(); }

    hba.pw32(p, port_off::CMD, hba.pr32(p, port_off::CMD) | CMD_FRE);
    hba.pw32(p, port_off::CMD, hba.pr32(p, port_off::CMD) | CMD_ST);

    for _ in 0..100_000 {
        if hba.pr32(p, port_off::SSTS) & SSTS_DET_MASK == SSTS_DET_ESTAB { break; }
        core::hint::spin_loop();
    }
    if hba.pr32(p, port_off::SSTS) & SSTS_DET_MASK != SSTS_DET_ESTAB {
        return Err("no device");
    }

    let mut port = AhciPort {
        hba: *hba, port: p,
        _cl_paddr: cl_paddr, cl_vaddr: cl_paddr,
        scratch_paddr: sc_paddr, scratch_vaddr: sc_paddr,
        max_prdt, n_slots,
        sector_count: 0, lba48: false, model: [0u8; 40],
        slots,
        slot_alloc: core::sync::atomic::AtomicU32::new(0),
    };

    port.identify()?;
    Ok(port)
}

pub fn device() -> Option<Arc<dyn BlockDevice>> {
    let g = PORT.lock();
    g.as_ref().map(|p| p.clone() as Arc<dyn BlockDevice>)
}
