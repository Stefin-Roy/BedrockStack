//! VT-d Second-Level Page Table (SLPT) walker.
//!
//! Fully spec-compliant second-level page table walker supporting
//! AGAW 39 (3 levels, 512GB) and AGAW 48 (4 levels, 256TB).

use crate::mm::phys_alloc::BitmapAllocator;

// Second-Level Paging Entry Bits (VT-d Spec §9.3 & Table 9-4..9-7)
pub const PTE_READ: u64 = 1 << 0;
pub const PTE_WRITE: u64 = 1 << 1;
pub const PTE_PS: u64 = 1 << 7;   // Page Size (1=Large Page at PD/PDP)
pub const PTE_A: u64 = 1 << 8;    // Accessed (if CAP.ADS=1)
pub const PTE_D: u64 = 1 << 9;    // Dirty (if CAP.ADS=1)
pub const PTE_SNP: u64 = 1 << 11; // Snoop (Leaf-only, if ECAP.SC=1)

pub const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000; // Bits 51:12

#[inline]
fn sl_pte(phys: u64, flags: u64) -> u64 {
    (phys & PTE_ADDR_MASK) | (flags & (PTE_READ | PTE_WRITE | PTE_SNP | PTE_A | PTE_D | PTE_PS))
}

#[inline]
fn pte_frame(entry: u64) -> u64 {
    entry & PTE_ADDR_MASK
}

#[inline]
fn pte_deref(frame: u64) -> *mut u64 {
    crate::mm::layout::to_physmap(frame) as *mut u64
}

#[inline]
unsafe fn read_pte(table: *mut u64, idx: usize) -> u64 {
    unsafe { *table.add(idx & 0x1FF) }
}

#[inline]
unsafe fn write_pte(table: *mut u64, idx: usize, val: u64) {
    unsafe { *table.add(idx & 0x1FF) = val }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agaw {
    /// 39-bit guest address width: 3 levels (512 GiB)
    Level3,
    /// 48-bit guest address width: 4 levels (256 TiB)
    Level4,
}

impl Agaw {
    pub fn levels(self) -> usize {
        match self {
            Agaw::Level3 => 3,
            Agaw::Level4 => 4,
        }
    }

    pub fn max_iova(self) -> u64 {
        match self {
            Agaw::Level3 => (1u64 << 39) - 1,
            Agaw::Level4 => (1u64 << 48) - 1,
        }
    }

    pub fn context_aw_field(self) -> u64 {
        match self {
            Agaw::Level3 => 1,
            Agaw::Level4 => 2,
        }
    }

    pub fn sagaw_bit(self) -> u64 {
        match self {
            Agaw::Level3 => 1 << 1,
            Agaw::Level4 => 1 << 2,
        }
    }
}

fn alloc_table(alloc: &mut BitmapAllocator) -> Option<u64> {
    let phys = alloc.alloc()?;
    unsafe {
        core::ptr::write_bytes(crate::mm::layout::to_physmap(phys) as *mut u8, 0, 4096);
    }
    Some(phys)
}

/// Map a single 4K page: `iova` -> `phys`.
pub fn map_4k(
    root: u64,
    alloc: &mut BitmapAllocator,
    iova: u64,
    phys: u64,
    agaw: Agaw,
    allow_snp: bool,
) -> Result<(), &'static str> {
    if (iova & 0xFFF) != 0 || (phys & 0xFFF) != 0 {
        return Err("IOMMU SLPT: unaligned address");
    }
    if iova > agaw.max_iova() {
        return Err("IOMMU SLPT: IOVA exceeds AGAW limit");
    }
    if (phys & !PTE_ADDR_MASK) != 0 {
        return Err("IOMMU SLPT: physical address out of range");
    }

    let levels = agaw.levels();
    let idx = |level: usize, va: u64| -> usize {
        let shift = 12 + level * 9;
        ((va >> shift) & 0x1FF) as usize
    };

    let mut cur = root;
    for lvl in (1..levels).rev() {
        let i = idx(lvl, iova);
        let table = pte_deref(cur);
        let entry = unsafe { read_pte(table, i) };

        if (entry & (PTE_READ | PTE_WRITE)) == 0 {
            // Allocate next level table (non-leaf has no SNP or PS)
            let next = alloc_table(alloc).ok_or("IOMMU SLPT OOM")?;
            let new_entry = sl_pte(next, PTE_READ | PTE_WRITE);
            unsafe { write_pte(table, i, new_entry) };
            cur = next;
        } else {
            // Superpages are not supported for 4K walks — return error instead of orphaning subtree.
            if (entry & PTE_PS) != 0 {
                return Err("IOMMU SLPT: non-leaf superpage collision");
            }
            let nxt = pte_frame(entry);
            if nxt == 0 {
                return Err("IOMMU SLPT: invalid intermediate table pointer");
            }
            cur = nxt;
        }
    }

    let leaf_i = idx(0, iova);
    let table = pte_deref(cur);
    let existing = unsafe { read_pte(table, leaf_i) };
    if (existing & (PTE_READ | PTE_WRITE)) != 0 {
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

/// Map a contiguous range with transactional rollback on failure.
pub fn map_range(
    root: u64,
    alloc: &mut BitmapAllocator,
    iova: u64,
    phys: u64,
    size: u64,
    agaw: Agaw,
    allow_snp: bool,
) -> Result<(), &'static str> {
    if (iova & 0xFFF) != 0 || (phys & 0xFFF) != 0 || (size & 0xFFF) != 0 {
        return Err("IOMMU SLPT: map_range unaligned arguments");
    }
    if iova > agaw.max_iova() || iova.checked_add(size).map_or(true, |end| end - 1 > agaw.max_iova()) {
        return Err("IOMMU SLPT: range exceeds AGAW limit");
    }
    let pages = size / 4096;
    for p in 0..pages {
        let cur_iova = iova + p * 4096;
        let cur_phys = phys + p * 4096;
        if let Err(e) = map_4k(root, alloc, cur_iova, cur_phys, agaw, allow_snp) {
            // Roll back previously mapped pages
            for rollback_p in 0..p {
                unmap_4k(root, iova + rollback_p * 4096, agaw);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Unmap a single 4 KiB IOVA page.
pub fn unmap_4k(root: u64, iova: u64, agaw: Agaw) -> bool {
    if (iova & 0xFFF) != 0 || iova > agaw.max_iova() {
        return false;
    }
    let levels = agaw.levels();
    let idx = |level: usize, va: u64| -> usize {
        let shift = 12 + level * 9;
        ((va >> shift) & 0x1FF) as usize
    };

    let mut cur = root;
    for lvl in (1..levels).rev() {
        let table = pte_deref(cur);
        let entry = unsafe { read_pte(table, idx(lvl, iova)) };
        if (entry & (PTE_READ | PTE_WRITE)) == 0 || (entry & PTE_PS) != 0 {
            return false;
        }
        cur = pte_frame(entry);
        if cur == 0 {
            return false;
        }
    }

    let table = pte_deref(cur);
    let leaf_i = idx(0, iova);
    let leaf = unsafe { read_pte(table, leaf_i) };
    if (leaf & (PTE_READ | PTE_WRITE)) == 0 {
        return false;
    }
    unsafe { write_pte(table, leaf_i, 0) };
    true
}

/// Translate IOVA to physical address via SLPT walk (handles 4K pages and 2M/1G superpages).
pub fn translate(root: u64, iova: u64, agaw: Agaw) -> Option<u64> {
    if iova > agaw.max_iova() {
        return None;
    }
    let levels = agaw.levels();
    let idx = |level: usize, va: u64| -> usize {
        let shift = 12 + level * 9;
        ((va >> shift) & 0x1FF) as usize
    };

    let mut cur = root;
    for lvl in (1..levels).rev() {
        let table = pte_deref(cur);
        let entry = unsafe { read_pte(table, idx(lvl, iova)) };
        if (entry & (PTE_READ | PTE_WRITE)) == 0 {
            return None;
        }
        // Handle 2MB / 1GB superpages safely — do not dereference data page as table.
        if (entry & PTE_PS) != 0 {
            let page_size_mask = (1u64 << (12 + lvl * 9)) - 1;
            return Some((entry & !page_size_mask) | (iova & page_size_mask));
        }
        cur = pte_frame(entry);
        if cur == 0 {
            return None;
        }
    }

    let table = pte_deref(cur);
    let leaf = unsafe { read_pte(table, idx(0, iova)) };
    if (leaf & (PTE_READ | PTE_WRITE)) == 0 {
        return None;
    }
    Some(pte_frame(leaf) | (iova & 0xFFF))
}

/// Check if an IOVA is mapped.
pub fn is_mapped(root: u64, iova: u64, agaw: Agaw) -> bool {
    translate(root, iova, agaw).is_some()
}
