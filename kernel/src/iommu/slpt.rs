//! VT-d Second-Level Page Table (SLPT) walker.
//!
//! Mirrors `mm/vmm/x86_64.rs` raw walk but for VT-d PTE format.
//! Supports AGAW 39 (3 levels) and 48 (4 levels). Pages are 4K only
//! (no superpages for simplicity, robust for DMA).

use crate::mm::phys_alloc::BitmapAllocator;

// VT-d spec Table47 p9-32 and §3.7 reserved bits:
// SNP@11 for SS-PTE leaf only when ECAP.SC=1; non-leaf SNP is reserved when R/W=1.
// SC (Snoop Control) is ECAP[7]; when 0, SNP is reserved even for leaf.
// We defer SNP decision to caller via `allow_snp` (global SC).
const PTE_READ: u64 = 1 << 0;
const PTE_WRITE: u64 = 1 << 1;
const PTE_SNP: u64 = 1 << 11;
const PTE_A: u64 = 1 << 8;
const PTE_D: u64 = 1 << 9;
const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Second-level PTE build helper. `phys` must be HAW-masked outside.
#[inline]
fn sl_pte(phys: u64, flags: u64) -> u64 {
    (phys & PTE_ADDR_MASK) | (flags & (PTE_READ | PTE_WRITE | PTE_SNP | PTE_A | PTE_D))
}

#[inline]
fn pte_frame(entry: u64) -> u64 {
    entry & PTE_ADDR_MASK
}

#[inline]
fn pte_deref(frame: u64) -> *mut u64 {
    (crate::mm::layout::to_physmap(frame)) as *mut u64
}

#[inline]
unsafe fn read_pte(table: *mut u64, idx: usize) -> u64 {
    unsafe { *table.add(idx & 0x1FF) }
}

#[inline]
unsafe fn write_pte(table: *mut u64, idx: usize, val: u64) {
    unsafe { *table.add(idx & 0x1FF) = val }
}

/// SLPT levels for AGAW 48: 9-9-9-9-12 (PML4, PDP, PD, PT).
/// For AGAW 39: 9-9-9-12 (PDP, PD, PT) — we treat missing top as 0.

#[derive(Debug, Clone, Copy)]
pub enum Agaw {
    /// 39-bit guest address width: 3 levels.
    Level3,
    /// 48-bit: 4 levels.
    Level4,
}

impl Agaw {
    pub fn levels(self) -> usize {
        match self {
            Agaw::Level3 => 3,
            Agaw::Level4 => 4,
        }
    }
    /// Encode for context entry AW field (bits 2:0 per spec: 0=30bit,1=39,2=48,...)
    pub fn context_aw_field(self) -> u64 {
        match self {
            Agaw::Level3 => 1,
            Agaw::Level4 => 2,
        }
    }
    /// Encode for RTADDR `SAGAW` selection? Not needed; root selects context.
    /// For GSTS/CAP SAGAW bits, the index is per CAP_SAGAW.
    pub fn sagaw_bit(self) -> u64 {
        match self {
            Agaw::Level3 => 1 << 1, // CAP SAGAW bit1: 39
            Agaw::Level4 => 1 << 2, // bit2: 48
        }
    }
}

/// Allocate a zeroed 4K page table frame.
fn alloc_table(alloc: &mut BitmapAllocator) -> Option<u64> {
    let phys = alloc.alloc()?;
    unsafe {
        core::ptr::write_bytes(crate::mm::layout::to_physmap(phys) as *mut u8, 0, 4096);
    }
    Some(phys)
}

/// Map `[iova, iova+size)` to `[phys, phys+size)` in the SLPT rooted at `root`.
/// Both addresses and size must be 4K-aligned. Single 4K page per call.
/// `allow_snp` reflects ECAP.SC — when false, SNP is reserved and must be 0 even for leaf.
pub fn map_4k(
    root: u64,
    alloc: &mut BitmapAllocator,
    iova: u64,
    phys: u64,
    agaw: Agaw,
    allow_snp: bool,
) -> Result<(), &'static str> {
    assert_eq!(iova & 0xFFF, 0);
    assert_eq!(phys & 0xFFF, 0);
    // walk
    let levels = agaw.levels();
    // indices: for L4: i3 = bits 39-47, i2 30-38, i1 21-29, i0 12-20
    // for L3: i2 30-38, i1 21-29, i0 12-20
    let idx = |level: usize, va: u64| -> usize {
        // level 0 = PT (12), 1=PD(21),2=PDP(30),3=PML4(39)
        let shift = 12 + level * 9;
        ((va >> shift) & 0x1FF) as usize
    };
    let mut cur = root;
    // Walk non-leaf levels. Non-leaf SNP/A/D are reserved when R/W set (spec §3.7, QEMU
    // checks at level 1 with sspte 0xe88803). If a prior leaf at this level exists
    // (e.g., stale 2M leaf), treat as error rather than dereferencing garbage.
    for lvl in (1..levels).rev() {
        let i = idx(lvl, iova);
        let table = pte_deref(cur);
        let entry = unsafe { read_pte(table, i) };
        if entry & (PTE_READ | PTE_WRITE) == 0 {
            // allocate next level — no SNP/A/D for non-leaf (reserved)
            let next = alloc_table(alloc).ok_or("IOMMU SLPT OOM")?;
            let new_entry = sl_pte(next, PTE_READ | PTE_WRITE);
            unsafe { write_pte(table, i, new_entry) };
            cur = next;
        } else {
            // Existing entry must be a table pointer, not a leaf. Leaf at this
            // level would be a superpage (2M/1G) which we never create for 4K.
            // Check for SNP/PS reserve: if SNP set at non-leaf, it's a prior
            // buggy leaf (e.g., 0xe88803 at PD) — clear and replace with table.
            if entry & PTE_SNP != 0 {
                crate::drivers::serial::SerialPort::puts("[iommu] SLPT non-leaf SNP set, fixing lvl=");
                crate::drivers::serial::SerialPort::put_u64(lvl as u64);
                crate::drivers::serial::SerialPort::puts(" i=");
                crate::drivers::serial::SerialPort::put_u64(i as u64);
                crate::drivers::serial::SerialPort::puts(" entry=");
                crate::drivers::serial::SerialPort::put_hex(entry);
                crate::drivers::serial::SerialPort::puts("\n");
                // Overwrite with correct table entry (leaks the old leaf's page, but recovers)
                let next = alloc_table(alloc).ok_or("IOMMU SLPT OOM")?;
                let new_entry = sl_pte(next, PTE_READ | PTE_WRITE);
                unsafe { write_pte(table, i, new_entry) };
                cur = next;
            } else {
                let nxt = pte_frame(entry);
                if nxt == 0 {
                    return Err("IOMMU SLPT: zero next frame");
                }
                cur = nxt;
            }
        }
    }
    // leaf PT level 0 — snoop controls cache coherency (VT-d §3.7, §11.4.3 ECAP.SC).
    // PD/PDP/PML4 are non-leaf and must have SNP=0 when R/W=1 (reserved).
    // PT leaf SNP is only allowed when ECAP.SC=1. QEMU's default intel-iommu
    // reports SC=0 (ecap 0xf00f0a), so SNP=1 causes "reserve non-zero" fault
    // at PD level if stale entry leaked. Gate leaf SNP on allow_snp.
    let leaf_i = idx(0, iova);
    let table = pte_deref(cur);
    let existing = unsafe { read_pte(table, leaf_i) };
    if existing & (PTE_READ | PTE_WRITE) != 0 {
        return Err("IOMMU SLPT: iova already mapped");
    }
    let leaf_flags = if allow_snp {
        PTE_READ | PTE_WRITE | PTE_SNP
    } else {
        PTE_READ | PTE_WRITE
    };
    let pte = sl_pte(phys, leaf_flags);
    unsafe { write_pte(table, leaf_i, pte) };
    Ok(())
}

/// Convenience: map a multi-page range (4K granularity).
pub fn map_range(
    root: u64,
    alloc: &mut BitmapAllocator,
    iova: u64,
    phys: u64,
    size: u64,
    agaw: Agaw,
    allow_snp: bool,
) -> Result<(), &'static str> {
    assert_eq!(iova & 0xFFF, 0);
    assert_eq!(phys & 0xFFF, 0);
    assert_eq!(size & 0xFFF, 0);
    let pages = size / 4096;
    for p in 0..pages {
        map_4k(
            root,
            alloc,
            iova + p * 4096,
            phys + p * 4096,
            agaw,
            allow_snp,
        )?;
    }
    Ok(())
}

/// Translate IOVA to phys via SLPT walk. Returns None if not mapped.
pub fn translate(root: u64, iova: u64, agaw: Agaw) -> Option<u64> {
    let levels = agaw.levels();
    let idx = |level: usize, va: u64| -> usize {
        let shift = 12 + level * 9;
        ((va >> shift) & 0x1FF) as usize
    };
    let mut cur = root;
    for lvl in (1..levels).rev() {
        let table = pte_deref(cur);
        let entry = unsafe { read_pte(table, idx(lvl, iova)) };
        if entry & (PTE_READ | PTE_WRITE) == 0 {
            return None;
        }
        cur = pte_frame(entry);
        if cur == 0 {
            return None;
        }
    }
    let table = pte_deref(cur);
    let leaf = unsafe { read_pte(table, idx(0, iova)) };
    if leaf & (PTE_READ | PTE_WRITE) == 0 {
        return None;
    }
    Some(pte_frame(leaf) | (iova & 0xFFF))
}

/// Check if a page is mapped.
pub fn is_mapped(root: u64, iova: u64, agaw: Agaw) -> bool {
    translate(root, iova, agaw).is_some()
}
