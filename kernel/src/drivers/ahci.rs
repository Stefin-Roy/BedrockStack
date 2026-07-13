//! AHCI (Advanced Host Controller Interface) SATA driver.
//!
//! Polling-mode driver for the Q35 ICH9 AHCI controller.
//!
//! Features:
//!   - Proper timeout via APIC timer count
//!   - TFD error checking + SERR diagnostics
//!   - Port reset recovery on command failure
//!   - Zero-copy DMA: PRDT points directly to caller buffer pages
//!   - Multi-PRDT for large transfers (up to 64 pages)
//!   - Proper BAR type detection (32/64-bit MMIO)
//!   - Proper MMIO region sizing from CAP.NP

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{Vmm, PageFlags, KERNEL_VMA_BASE};
use crate::platform::x86_64_pc::apic;
use super::traits::BlockDevice;

// ── Register offsets ────────────────────────────────────────────

#[allow(dead_code)]
mod ghc {
    pub const CAP: u32 = 0x00;
    pub const GHC: u32 = 0x04;
    pub const IS: u32 = 0x08;
    pub const PI: u32 = 0x0C;
    pub const VS: u32 = 0x10;
}

#[allow(dead_code)]
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
    pub const CI: u32 = 0x38;
}

// ── Bitfield constants ──────────────────────────────────────────

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

const MAX_PRDT: usize = 64;

// ── MMIO access ─────────────────────────────────────────────────

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
    let va = s.next_vaddr - size;
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

static PORT: Mutex<Option<AhciPort>> = Mutex::new(None);

struct AhciPort {
    hba: Hba,
    port: u8,
    cl_paddr: u64,
    cl_vaddr: u64,
    scratch_paddr: u64,
    scratch_vaddr: u64,
    max_prdt: usize,
    sector_count: u64,
    lba48: bool,
    model: [u8; 40],
}

unsafe impl Sync for AhciPort {}

// ── Low-level helpers ───────────────────────────────────────────

impl AhciPort {
    /// Wait for command slot with 5s timeout using APIC count.
    fn wait_slot(&self, slot: u8) -> Result<(), &'static str> {
        let deadline = ms_to_ticks(5000);
        let start = curr_count();
        loop {
            let ci = self.hba.pr32(self.port, port_off::CI);
            if ci & (1 << slot) == 0 {
                let tfd = self.hba.pr32(self.port, port_off::TFD);
                if tfd & TFD_ERR != 0 {
                    self.dump_err(slot, tfd as u8, self.hba.pr32(self.port, port_off::SERR));
                    return Err("AHCI cmd error");
                }
                return Ok(());
            }
            if elapsed_ticks(start) >= deadline {
                let tfd = self.hba.pr32(self.port, port_off::TFD);
                let serr = self.hba.pr32(self.port, port_off::SERR);
                self.dump_err(slot, tfd as u8, serr);
                return Err("AHCI timeout");
            }
            core::hint::spin_loop();
        }
    }

    fn dump_err(&self, _slot: u8, err: u8, serr: u32) {
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

    /// Build PRDT entries from a virtual buffer.
    /// Writes up to `self.max_prdt` entries starting at `prdt_ptr`.
    /// Returns number of entries written.
    fn build_prdt(&self, buf_vaddr: u64, size: usize, prdt_ptr: *mut PrdEntry) -> Result<usize, &'static str> {
        let mut rem = size as isize;
        let mut off: isize = 0;
        let mut n = 0usize;
        while rem > 0 && n < self.max_prdt {
            let va = (buf_vaddr as isize + off) as u64;
            let pa = translate(va & !0xFFF).ok_or("PRDT translate fail")?;
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

    /// Write a Register H2D FIS for a READ/WRITE DMA EXT command.
    fn write_fis(&self, fis_base: u64, lba: u64, count: u32, cmd: u8) {
        let f = fis_base as *mut u8;
        unsafe {
            f.add(0).write_volatile(0x27);      // type
            f.add(1).write_volatile(0x80);      // C=1
            f.add(2).write_volatile(cmd);
            f.add(3).write_volatile(0);
            f.add(4).write_volatile(lba as u8);
            f.add(5).write_volatile((lba >> 8) as u8);
            f.add(6).write_volatile((lba >> 16) as u8);
            f.add(7).write_volatile(if self.lba48 { 0x40 } else { 0xE0 });
            f.add(8).write_volatile((lba >> 24) as u8);
            f.add(9).write_volatile(if self.lba48 { (lba >> 32) as u8 } else { 0 });
            f.add(10).write_volatile(0);
            f.add(11).write_volatile(count as u8);
            f.add(12).write_volatile(if self.lba48 { (count >> 8) as u8 } else { 0 });
            f.add(13).write_volatile(0);
            f.add(14).write_volatile(0);
            f.add(15).write_volatile(0);
            for i in 16..20 { f.add(i).write_volatile(0); }
        }
    }

    // ── Command execution ──────────────────────────────────────
    //
    // Layout in the CL page (4K):
    //   0x000        CmdHeader for slot 0  (32 B)
    //   0x020-0x0FF  reserved for slots 1-31
    //   0x100        Command Table:
    //                0x100  Register H2D FIS  (64 B)
    //                0x140  ATAPI (unused)    (16 B)
    //                0x150  reserved          (48 B)
    //                0x180  PRDT entries      (16 B each)

    /// Issue a DMA command. If `buf_vaddr` is 0, uses the scratch buffer.
    fn issue_cmd(&self, lba: u64, count: u32, buf_vaddr: u64, size: usize, is_write: bool) -> Result<(), &'static str> {
        let slot: u8 = 0;
        let cl = self.cl_vaddr;
        let ctba = self.cl_paddr + 0x100;

        for _ in 0..100_000 {
            if self.hba.pr32(self.port, port_off::CI) & (1 << slot) == 0 { break; }
            core::hint::spin_loop();
        }

        self.hba.pw32(self.port, port_off::IS, !0);
        unsafe { core::ptr::write_bytes(cl as *mut u8, 0, 512); }

        // FIS.
        let cmd_byte = if is_write { 0x35u8 } else { 0x25u8 };
        self.write_fis(cl + 0x100, lba, count, cmd_byte);

        // PRDT.
        let prdt = (cl + 0x180) as *mut PrdEntry;
        let prdtl = if buf_vaddr != 0 {
            self.build_prdt(buf_vaddr, size, prdt)?
        } else {
            unsafe {
                (*prdt).dba = self.scratch_paddr as u32;
                (*prdt).dbau = (self.scratch_paddr >> 32) as u32;
                (*prdt)._rsvd = 0;
                (*prdt).dbc = (size - 1) as u32;
            }
            1usize
        };

        // CmdHeader.
        let hdr = cl as *mut CmdHeader;
        let w = if is_write { 1u32 } else { 0u32 };
        unsafe {
            (*hdr).cfl_w_prdtl = 5u32 | (w << 7) | (prdtl as u32) << 16;
            (*hdr).prdbc = 0;
            (*hdr).ctba = ctba as u32;
            (*hdr).ctbau = (ctba >> 32) as u32;
        }

        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        self.hba.pw32(self.port, port_off::CI, 1 << slot);

        match self.wait_slot(slot) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.port_reset()?;
                // Retry once after reset.
                self.hba.pw32(self.port, port_off::IS, !0);
                unsafe { core::ptr::write_bytes(cl as *mut u8, 0, 512); }
                self.write_fis(cl + 0x100, lba, count, cmd_byte);
                let prdt = (cl + 0x180) as *mut PrdEntry;
                let prdtl = if buf_vaddr != 0 {
                    self.build_prdt(buf_vaddr, size, prdt)?
                } else {
                    unsafe {
                        (*prdt).dba = self.scratch_paddr as u32;
                        (*prdt).dbau = (self.scratch_paddr >> 32) as u32;
                        (*prdt)._rsvd = 0;
                        (*prdt).dbc = (size - 1) as u32;
                    }
                    1usize
                };
                let hdr = cl as *mut CmdHeader;
                let w = if is_write { 1u32 } else { 0u32 };
                unsafe {
                    (*hdr).cfl_w_prdtl = 5u32 | (w << 7) | (prdtl as u32) << 16;
                    (*hdr).prdbc = 0;
                    (*hdr).ctba = ctba as u32;
                    (*hdr).ctbau = (ctba >> 32) as u32;
                }
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
                self.hba.pw32(self.port, port_off::CI, 1 << slot);
                self.wait_slot(slot)
            }
        }
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
        let fis = self.cl_vaddr + 0x100;
        unsafe { core::ptr::write_bytes(self.cl_vaddr as *mut u8, 0, 512); }
        self.write_fis(fis, 0, 0, 0xEC);

        let prdt = (self.cl_vaddr + 0x180) as *mut PrdEntry;
        unsafe {
            (*prdt).dba = self.scratch_paddr as u32;
            (*prdt).dbau = (self.scratch_paddr >> 32) as u32;
            (*prdt)._rsvd = 0;
            (*prdt).dbc = 511;
        }

        let hdr = self.cl_vaddr as *mut CmdHeader;
        unsafe {
            (*hdr).cfl_w_prdtl = 5u32 | (1u32 << 16);
            (*hdr).prdbc = 0;
            (*hdr).ctba = (self.cl_paddr + 0x100) as u32;
            (*hdr).ctbau = ((self.cl_paddr + 0x100) >> 32) as u32;
        }

        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        self.hba.pw32(self.port, port_off::CI, 1);
        self.wait_slot(0)?;

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
    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        let bytes = (count as usize) * 512;
        if buf.len() < bytes { return Err("buf too small"); }

        let va = buf.as_ptr() as u64;
        // Try zero-copy directly into caller's buffer pages.
        match self.issue_cmd(lba, count, va, bytes, false) {
            Ok(()) => return Ok(()),
            Err(_) => {}
        }

        // Fallback: scratch buffer + copy.
        if count > 8 { return Err("max 8 sectors (fallback)"); }
        self.issue_cmd(lba, count, 0, bytes, false)?;
        unsafe { core::ptr::copy_nonoverlapping(self.scratch_vaddr as *const u8, buf.as_mut_ptr(), bytes); }
        Ok(())
    }

    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), &'static str> {
        let bytes = (count as usize) * 512;
        if buf.len() < bytes { return Err("buf too small"); }

        let va = buf.as_ptr() as u64;
        match self.issue_cmd(lba, count, va, bytes, true) {
            Ok(()) => return Ok(()),
            Err(_) => {}
        }

        if count > 8 { return Err("max 8 sectors (fallback)"); }
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), self.scratch_vaddr as *mut u8, bytes); }
        self.issue_cmd(lba, count, 0, bytes, true)
    }

    fn sector_count(&self) -> u64 { self.sector_count }
    fn model_string(&self) -> &str {
        let t = core::str::from_utf8(&self.model).unwrap_or("(bad utf8)");
        t.trim_end_matches(char::from(0))
    }
}

// ── Initialisation ──────────────────────────────────────────────

const VMM_VADDR: u64 = KERNEL_VMA_BASE - 0x10000000 - 0x20000000 - 0x20000000;

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
    let n_slots = ((cap >> 8) & 0x1F) + 1;
    let max_prdt_raw = (4096 - 0x180) / 16;

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
    SerialPort::put_u64(n_slots as u64);
    SerialPort::puts("\n");

    // HBA reset.
    mmio.w32(ghc::GHC, GHC_HR);
    for _ in 0..1000 {
        if mmio.r32(ghc::GHC) & GHC_HR == 0 { break; }
        core::hint::spin_loop();
    }
    if mmio.r32(ghc::GHC) & GHC_HR != 0 {
        SerialPort::puts("[ahci] HBA reset timeout\n"); return;
    }
    mmio.w32(ghc::GHC, mmio.r32(ghc::GHC) | GHC_AE);

    // Probe.
    for p in 0u8..n_ports.min(32) as u8 {
        if pi & (1 << p) == 0 { continue; }
        let ssts = mmio.pr32(p, port_off::SSTS);
        if ssts & SSTS_DET_MASK != SSTS_DET_ESTAB { continue; }
        if (ssts >> 8) & 0x0F != 1 { continue; }
        if mmio.pr32(p, port_off::SIG) != 0x0000_0101 { continue; }

        match init_one(p, &mmio, alloc, max_prdt_raw.min(MAX_PRDT)) {
            Ok(port) => {
                *PORT.lock() = Some(port);
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
    if bars[5] & 1 != 0 { return (0, false); } // I/O
    match bars[5] & 0x06 {
        0 => ((bars[5] & 0xFFFF_FFF0) as u64, true),   // 32-bit
        4 => {
            let base = ((bars[5] as u64) & 0xFFFF_FFF0) | ((bars[4] as u64) << 32);
            (base, true)
        }
        _ => (0, false),
    }
}

fn init_one(p: u8, hba: &Hba, alloc: *mut BitmapAllocator, max_prdt: usize) -> Result<AhciPort, &'static str> {
    let alloc = unsafe { &mut *alloc };

    let cmd = hba.pr32(p, port_off::CMD);
    hba.pw32(p, port_off::CMD, cmd & !(CMD_ST | CMD_FRE));
    for _ in 0..1000 {
        if hba.pr32(p, port_off::CMD) & (CMD_CR | CMD_FR) == 0 { break; }
        core::hint::spin_loop();
    }

    let cl = alloc.alloc().ok_or("OOM CL")?;
    let sc = alloc.alloc().ok_or("OOM scratch")?;
    unsafe {
        core::ptr::write_bytes(cl as *mut u8, 0, 4096);
        core::ptr::write_bytes(sc as *mut u8, 0, 4096);
    }

    hba.pw32(p, port_off::CLB, cl as u32);
    hba.pw32(p, port_off::CLBU, (cl >> 32) as u32);
    hba.pw32(p, port_off::FB, sc as u32);
    hba.pw32(p, port_off::FBU, (sc >> 32) as u32);

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
        cl_paddr: cl, cl_vaddr: cl,
        scratch_paddr: sc, scratch_vaddr: sc,
        max_prdt,
        sector_count: 0, lba48: false, model: [0u8; 40],
    };

    port.identify()?;
    Ok(port)
}

pub fn device() -> &'static dyn BlockDevice {
    let g = PORT.lock();
    let p = g.as_ref().expect("ahci not init");
    unsafe { core::mem::transmute::<&AhciPort, &'static AhciPort>(p) }
}
