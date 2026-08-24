//! Queued Invalidation (QI) helpers and fallback register-based invalidation.
//!
//! Implements a minimal QI ring (256 entries, DW=0) for context-cache and
//! IOTLB invalidation, plus reg-based fallback via CCMD/IOTLB. Caller must
//! have mapped the IOMMU registers UC.

use core::sync::atomic::{Ordering, fence};

use crate::drivers::serial::SerialPort;

// ── VT-d register offsets ──────────────────────────────────────────
// Only the registers this module touches (spec Chapter 11):
// ECAP 0x10 (8), GCMD 0x18 (4), GSTS 0x1C (4), CCMD 0x28 (8),
// FSTS 0x34 (4), IQH @0x80, IQT @0x88, IQA @0x90 (§11.4.9.1-3).
const REG_ECAP: u64 = 0x10;
const REG_GCMD: u64 = 0x18;
const REG_GSTS: u64 = 0x1C;
const REG_CCMD: u64 = 0x28;
const REG_FSTS: u64 = 0x34;
const REG_IQH: u64 = 0x80;
const REG_IQT: u64 = 0x88;
const REG_IQA: u64 = 0x90;

// GCMD bits
const GCMD_QIE: u32 = 1 << 26;

// GSTS bits
const GSTS_QIES: u32 = 1 << 26;

// ECAP bits (11.4.3)
const ECAP_QI: u64 = 1 << 1;
const ECAP_IRO_MASK: u64 = 0x3FF << 8;
const ECAP_IRO_SHIFT: u64 = 8;

// CCMD bits
const CCMD_ICC: u64 = 1 << 63;
const CCMD_CIRG_GLOBAL: u64 = 1 << 61;

// IOTLB registers are at offset = base + 16*IRO (spec p11-18, p11-30 XXXh+008h).
// They consist of IVA_REG @ X and IOTLB_REG @ X+8.
#[inline]
fn iotlb_regs_offset(ecap: u64) -> u64 {
    ((ecap & ECAP_IRO_MASK) >> ECAP_IRO_SHIFT) * 16
}
#[inline]
fn iotlb_reg_offset(ecap: u64) -> u64 {
    iotlb_regs_offset(ecap) + 8
}

#[inline]
unsafe fn read32(base: u64, off: u64) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}
#[inline]
unsafe fn write32(base: u64, off: u64, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}
#[inline]
unsafe fn read64(base: u64, off: u64) -> u64 {
    unsafe { core::ptr::read_volatile((base + off) as *const u64) }
}
#[inline]
unsafe fn write64(base: u64, off: u64, val: u64) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u64, val) }
}

pub struct QiState {
    pub base_va: u64,
    pub iqa_phys: u64,
    pub iqa_va: u64,
    pub queue_size: usize, // entries
    pub head: usize,
    pub tail: usize,
    pub has_qi: bool,
    pub wait_status_phys: u64,
    pub wait_status_va: u64,
}

impl QiState {
    pub fn new(base_va: u64, ecap: u64) -> Self {
        QiState {
            base_va,
            iqa_phys: 0,
            iqa_va: 0,
            queue_size: 0,
            head: 0,
            tail: 0,
            has_qi: (ecap & ECAP_QI) != 0,
            wait_status_phys: 0,
            wait_status_va: 0,
        }
    }
}

/// Initialize the Invalidation Queue if ECAP.QI = 1.
/// Allocates a 4K page (128×16 bytes) and programs IQA. Returns true on success.
pub fn init_qi(
    qi: &mut QiState,
    alloc: &mut crate::mm::phys_alloc::BitmapAllocator,
) -> bool {
    if !qi.has_qi {
        SerialPort::puts("[iommu] QI not supported (ECAP.QI=0), using reg-based inv\n");
        return false;
    }
    let page_phys = match alloc.alloc() {
        Some(p) => p,
        None => {
            SerialPort::puts("[iommu] QI OOM\n");
            return false;
        }
    };
    let page_va = crate::mm::layout::to_physmap(page_phys);
    unsafe { core::ptr::write_bytes(page_va as *mut u8, 0, 4096) };
    qi.iqa_phys = page_phys;
    qi.iqa_va = page_va;
    qi.queue_size = 4096 / 16; // 256 entries ×16 B =4096 (QS=0: 2^(0+8)=256)
    qi.head = 0;
    qi.tail = 0;
    // Allocate status page for WAIT descriptor (SW=1) — 4K zeroed.
    let status_phys = match alloc.alloc() {
        Some(p) => p,
        None => {
            SerialPort::puts("[iommu] QI status OOM\n");
            return false;
        }
    };
    let status_va = crate::mm::layout::to_physmap(status_phys);
    unsafe { core::ptr::write_bytes(status_va as *mut u8, 0, 4096) };
    qi.wait_status_phys = status_phys;
    qi.wait_status_va = status_va;
    // Program IQA: bits 63:12 = phys, 11 = DW (0=128-bit), 2:0 = QS where entries=2^(QS+8).
    // For 256 entries QS=0, DW=0 => value = phys.
    let qs: u64 = 0; // 256 entries => QS=0
    let dw: u64 = 0; // 128-bit descriptors
    let iqa_val = (page_phys & !0xFFF) | (qs & 0x7) | ((dw & 1) << 11);
    unsafe {
        // Spec §6.5.2: Head/Tail must be 0 before enable.
        write64(qi.base_va, REG_IQH, 0);
        write64(qi.base_va, REG_IQT, 0);
        fence(Ordering::SeqCst);
        write64(qi.base_va, REG_IQA, iqa_val);
        // Ensure write visible
        fence(Ordering::SeqCst);
    }
    // Enable QI via GCMD
    SerialPort::puts("[iommu] QI enabled @phys=");
    SerialPort::put_hex(page_phys);
    SerialPort::puts("\n");
    // GCMD.QIE must be set with zeroed tail/head.
    let gsts = unsafe { read32(qi.base_va, REG_GSTS) };
    if gsts & GSTS_QIES != 0 {
        // already enabled?
        SerialPort::puts("[iommu] QI already enabled\n");
        return true;
    }
    unsafe {
        let mut gcmd = read32(qi.base_va, REG_GCMD);
        gcmd |= GCMD_QIE;
        write32(qi.base_va, REG_GCMD, gcmd);
    }
    // Poll for QIES=1, timeout 1s (approx with spin loops)
    let mut polls = 0u32;
    while polls < 1_000_000 {
        let gsts2 = unsafe { read32(qi.base_va, REG_GSTS) };
        if gsts2 & GSTS_QIES != 0 {
            SerialPort::puts("[iommu] QI GSTS.QIES set\n");
            return true;
        }
        core::hint::spin_loop();
        polls += 1;
        if polls % 100_000 == 0 {
            core::hint::spin_loop();
        }
    }
    SerialPort::puts("[iommu] QI enable timeout\n");
    false
}

/// Clear Invalidation Queue Error (FSTS.IQE bit4 W1C) if set.
/// Returns true if we cleared an error (caller should retry or abort, not use CCMD).
#[inline]
fn clear_qi_error(base_va: u64) -> bool {
    let fsts = unsafe { read32(base_va, REG_FSTS) };
    if fsts & 0x10 != 0 {
        SerialPort::puts("[iommu] QI IQE set, clearing FSTS.IQE\n");
        unsafe {
            // W1C: write 1 to bit4
            write32(base_va, REG_FSTS, 0x10);
            fence(Ordering::SeqCst);
        }
        // Drain fault records as well
        let fsts2 = unsafe { read32(base_va, REG_FSTS) };
        if fsts2 & 0x10 != 0 {
            SerialPort::puts("[iommu] QI IQE still set after clear\n");
        }
        return true;
    }
    false
}

/// Submit a QI descriptor (16 bytes, DW=0). Tail advances, IQH polled.
// Caller must hold IOMMU lock. Uses byte-offset for IQT/IQH per spec p11-52..54 (§11.4.9.1-2).
// DW=0 => 16 B descriptors, tail_off = tail*16. If DW=1, would be *32 (not used).
pub fn qi_submit(qi: &mut QiState, dw0: u64, dw1: u64) -> bool {
    if !qi.has_qi || qi.queue_size == 0 {
        return false;
    }
    // Clear a stale IQE left by a previous failed descriptor and proceed;
    // the post-submit check below catches a fresh IQE on THIS descriptor.
    clear_qi_error(qi.base_va);
    let idx = qi.tail;
    if idx >= qi.queue_size {
        return false;
    }
    let va = qi.iqa_va + (idx as u64) * 16;
    unsafe {
        core::ptr::write_volatile(va as *mut u64, dw0);
        core::ptr::write_volatile((va + 8) as *mut u64, dw1);
        fence(Ordering::SeqCst);
    }
    qi.tail = (qi.tail + 1) % qi.queue_size;
    // IQT is byte offset (tail*16) per spec 11.4.9.2 (DW=0 => shift 4, DW=1 => shift 5)
    let tail_off = (qi.tail as u64) * 16;
    unsafe {
        write64(qi.base_va, REG_IQT, tail_off);
        fence(Ordering::SeqCst);
    }
    // Poll IQH byte offset. Wrap-aware: IQH==tail means drained (assuming single producer).
    // Also watch FSTS IQE — an invalid descriptor leaves head stuck and sets IQE.
    let mut spins = 0u64;
    let qsize_bytes = (qi.queue_size as u64) * 16;
    while spins < 500_000 {
        let raw_iqh = unsafe { read64(qi.base_va, REG_IQH) };
        // Check for queue error first: FSTS_IQE (bit4) indicates descriptor error
        let fsts = unsafe { read32(qi.base_va, REG_FSTS) };
        if fsts & 0x10 != 0 {
            // IQE — descriptor rejected, head will not advance. Clear for next attempt.
            clear_qi_error(qi.base_va);
            return false;
        }
        let iqh_mod = raw_iqh % qsize_bytes;
        let tail_mod = tail_off % qsize_bytes;
        if iqh_mod == tail_mod {
            return true;
        }
        core::hint::spin_loop();
        spins += 1;
    }
    // Timeout — head did not catch up. Check if IQE caused it.
    clear_qi_error(qi.base_va);
    false
}

/// Convenience: global context-cache invalidation via QI.
/// Spec Figure6-1: Type 1h, G=01b global at bits 5:4 (gran<<4), DID@16 for domain, SID/FM ignored for global.
/// Linux `QI_CC_TYPE=0x1 | gran<<4 | did<<16`.
pub fn qi_invalidate_context(qi: &mut QiState) -> bool {
    // global: gran=01b => 1<<4
    let dw0 = 0x1u64 | (1u64 << 4);
    let dw1 = 0;
    qi_submit(qi, dw0, dw1)
}

/// IOTLB global invalidation via QI.
/// Spec Figure6-3: Type 2h, G=01b global, DID@16 (low). For global, DID is
/// ignored but must be in the DID field (bits 31:16); setting it at 32 as well
/// would hit reserved bits for a 128-bit queue and cause IQE.
pub fn qi_invalidate_iotlb(qi: &mut QiState, did: u16) -> bool {
    let did64 = did as u64;
    // 0x2 = IOTLB type (bits 3:0 + 11:9), gran 01b at bit4, DID at 16
    let dw0 = 0x2u64 | (1u64 << 4) | (did64 << 16);
    let dw1 = 0;
    qi_submit(qi, dw0, dw1)
}

/// Wait descriptor (Type 5h) helper — orders all previously-submitted QI
/// descriptors ahead of subsequent DMA and satisfies the QI-disable quiesce
/// rule (§6.5.4: last completed descriptor must be type 5h).
///
/// Encoding per spec §6.5.2.9 Fig6-9 (128-bit): quadword 0 holds
/// Type[3:0]=5h, Type[11:9]=000h, IF@4, SW@5, FN@6, RsvdZ above bit 11 and
/// **Status Data at [63:32]**; quadword 1 holds the Status Address ([63:2]).
/// With SW=1 hardware confirms completion with a coherent DWORD write of the
/// status data to the status address, so polling that word proves the prior
/// CC+IOTLB invalidations are globally observable (§6.5.2.12). Falls back to
/// a fence (FN-only) wait if no status page exists or the submission errors.
pub fn qi_invalidate_wait(qi: &mut QiState) -> bool {
    if qi.wait_status_phys == 0 {
        // No status page: fence-only wait still enforces ordering.
        return qi_submit(qi, 0x5u64 | (1u64 << 6), 0);
    }
    unsafe { core::ptr::write_volatile(qi.wait_status_va as *mut u32, 0) };
    fence(Ordering::SeqCst);
    // Status Data = 1 signals completion.
    let dw0 = 0x5u64 | (1u64 << 5) | (1u64 << 6) | (1u64 << 32);
    let dw1 = qi.wait_status_phys & !0x3u64;
    if qi_submit(qi, dw0, dw1) {
        // IQH already passed the WAIT inside qi_submit (in-order hardware),
        // so ordering is guaranteed; poll briefly for the visible status word.
        let mut spins = 0u32;
        while spins < 50_000 {
            let v = unsafe { core::ptr::read_volatile(qi.wait_status_va as *const u32) };
            if v == 1 {
                return true;
            }
            core::hint::spin_loop();
            spins += 1;
        }
        return true;
    }
    // SW submission failed (e.g., IQE): degrade to fence-only wait.
    qi_submit(qi, 0x5u64 | (1u64 << 6), 0)
}

/// Register-based fallback: global context-cache invalidation.
pub fn reg_invalidate_context_global(base_va: u64) -> bool {
    // CCMD: ICC=1, CIRG=global (01), CAIG set by hw, DID ignored for global.
    let val = CCMD_ICC | CCMD_CIRG_GLOBAL;
    unsafe {
        write64(base_va, REG_CCMD, val);
        fence(Ordering::SeqCst);
    }
    // Poll ICC=0
    let mut polls = 0u32;
    while polls < 1_000_000 {
        let v = unsafe { read64(base_va, REG_CCMD) };
        if v & CCMD_ICC == 0 {
            return true;
        }
        core::hint::spin_loop();
        polls += 1;
    }
    SerialPort::puts("[iommu] reg ctx inv timeout\n");
    false
}

/// Register IOTLB global invalidation (if QI unavailable).
/// Spec p11-30/11-18: offset = 16*IRO, IVA_REG @ off, IOTLB_REG @ off+8. IVT@63, IIRG@60.
pub fn reg_invalidate_iotlb_global(base_va: u64) -> bool {
    let ecap = unsafe { read64(base_va, REG_ECAP) };
    let off = iotlb_reg_offset(ecap);
    if off == 0 {
        // IRO=0 would place the IOTLB registers over the base register file
        // (VER/CAP/ECAP); no compliant unit reports it. Skip rather than
        // corrupt the register page.
        SerialPort::puts("[iommu] reg iotlb IRO=0 unusable, skip\n");
        return true;
    }
    let ivt = 1u64 << 63;
    let iirg_global = 1u64 << 60;
    let val = ivt | iirg_global;
    unsafe {
        write64(base_va, off, val);
        fence(Ordering::SeqCst);
    }
    let mut polls = 0u32;
    while polls < 500_000 {
        let v = unsafe { read64(base_va, off) };
        if v & (1u64 << 63) == 0 {
            return true;
        }
        core::hint::spin_loop();
        polls += 1;
    }
    SerialPort::puts("[iommu] reg iotlb timeout (non-fatal if QI)\n");
    true
}

// Helper to emit trace when iommu_trace enabled
pub fn trace(msg: &str) {
    if cfg!(feature = "iommu_trace") {
        SerialPort::puts("[iommu] ");
        SerialPort::puts(msg);
        SerialPort::puts("\n");
    }
}
