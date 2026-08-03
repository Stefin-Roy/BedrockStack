//! x86_64 4-level page table operations.
//!
//! Wraps the `x86_64` crate's `OffsetPageTable`.  Before the DIRECT_MAP
//! physmap is enabled the offset is 0 (identity); once `init_physmap` runs the
//! offset becomes `PHYS_MAP_BASE`, so page-table frames are dereferenced
//! through the kernel-internal physmap rather than the identity map.

use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size2MiB,
    Size4KiB, Translate,
};
use x86_64::{PhysAddr as XPhysAddr, VirtAddr};

use crate::mm::phys_alloc::BitmapAllocator;
use super::PageFlags;

// ── Frame-allocator adapter ─────────────────────────────────────────

struct BitmapFrameAllocator<'a> {
    inner: &'a mut BitmapAllocator,
}

unsafe impl<'a> FrameAllocator<Size4KiB> for BitmapFrameAllocator<'a> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.inner
            .alloc()
            .map(|addr| PhysFrame::containing_address(XPhysAddr::new(addr)))
    }
}

// ── Private helpers ─────────────────────────────────────────────────

#[inline]
fn mapper_at<'a>(root: u64) -> OffsetPageTable<'a> {
    let off = crate::mm::layout::phys_offset();
    let root_ptr = root.wrapping_add(off) as *mut PageTable;
    unsafe { OffsetPageTable::new(&mut *root_ptr, VirtAddr::new(off)) }
}

#[inline]
fn page_flags_to_x86(flags: PageFlags) -> PageTableFlags {
    let mut f = PageTableFlags::PRESENT;
    if flags.contains(PageFlags::WRITE) {
        f |= PageTableFlags::WRITABLE;
    }
    if !flags.contains(PageFlags::EXECUTE) {
        f |= PageTableFlags::NO_EXECUTE;
    }
    if flags.contains(PageFlags::NO_CACHE) {
        // Need BOTH PCD and PWT for UC (PAT index 3 = 0 × 4 + 2 × 1 + 1 × 1).
        // With only PCD (bit 4), PAT index = 2 → WT (Write-Through),
        // which allows cache hits on reads giving stale data.
        f |= PageTableFlags::NO_CACHE;         // PCD
        f |= PageTableFlags::WRITE_THROUGH;    // PWT
    }
    if flags.contains(PageFlags::WRITE_COMBINING) {
        // PAT index 1 (001): PWT=1, PCD=0, PAT=0
        // Requires PAT MSR entry 1 = 01h (WC).
        f |= PageTableFlags::WRITE_THROUGH;
    }
    if flags.contains(PageFlags::USER) {
        f |= PageTableFlags::USER_ACCESSIBLE;
    }
    f
}

/// Program IA32_PAT MSR so that PAT entry 1 = WC (01h).
pub fn init_pat_wc() {
    use x86_64::registers::model_specific::Msr;
    const IA32_PAT: u32 = 0x277;
    let mut msr = Msr::new(IA32_PAT);
    let val = unsafe { msr.read() };
    // Entry 1 is bits 15:8. Change byte 1 to 01h (WC).
    let new_val = (val & !(0xFF << 8)) | (0x01u64 << 8);
    unsafe { msr.write(new_val); }
}

#[inline]
fn table_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::ACCESSED
}

// ── Public API ──────────────────────────────────────────────────────

pub fn map_4k(
    root: u64,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    paddr: u64,
    flags: PageFlags,
) {
    let mut mapper = mapper_at(root);
    let mut frame_alloc = BitmapFrameAllocator { inner: alloc };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::<Size4KiB>::containing_address(XPhysAddr::new(paddr));
    let x86_flags = page_flags_to_x86(flags);

    unsafe {
        mapper
            .map_to_with_table_flags(page, frame, x86_flags, table_flags(), &mut frame_alloc)
            .expect("x86_64 4KiB map failed")
            .flush();
    }
}

pub fn map_2m(
    root: u64,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    paddr: u64,
    flags: PageFlags,
) {
    let mut mapper = mapper_at(root);
    let mut frame_alloc = BitmapFrameAllocator { inner: alloc };

    let page = Page::<Size2MiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::<Size2MiB>::containing_address(XPhysAddr::new(paddr));
    let x86_flags = page_flags_to_x86(flags);

    unsafe {
        mapper
            .map_to_with_table_flags(page, frame, x86_flags, table_flags(), &mut frame_alloc)
            .expect("x86_64 2MiB map failed")
            .flush();
    }
}

pub fn unmap_4k(root: u64, alloc: &mut BitmapAllocator, vaddr: u64) -> bool {
    let mut mapper = mapper_at(root);

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let removed = match mapper.unmap(page) {
        Ok((_mapped_frame, flush)) => {
            flush.flush();
            // NB: `_mapped_frame` is the frame that was mapped at this VA and
            // is owned by the *caller* — freeing it here would release memory
            // another component may still be using.  We reclaim only the
            // intermediate (L1/L2/L3) tables, once they become empty.
            true
        }
        Err(_) => false,
    };
    if !removed {
        return false;
    }
    reclaim_empty_tables(root, alloc, vaddr);
    // Freeing intermediate tables drops valid working-set entries for the
    // whole address space, not just the single unmapped page.  Full flush.
    crate::mm::vmm::flush_tlb();
    true
}

/// The low 12 flag bits of an x86_64 PTE; bit 0 is PRESENT.
const PTE_PRESENT: u64 = 1 << 0;

/// Physical-frame bits of a page-table entry (bits 12..=51).
fn pte_frame(entry: u64) -> u64 {
    entry & 0x000F_FFFF_FFFF_F000
}

/// Virtual deref address for a page-table frame (through the private physmap).
fn pte_deref(frame: u64) -> *mut u64 {
    (crate::mm::layout::to_physmap(frame)) as *mut u64
}

#[inline]
unsafe fn read_pte(table: *mut u64, index: usize) -> u64 {
    unsafe { *table.add(index & 0x1FF) }
}

#[inline]
unsafe fn write_pte(table: *mut u64, index: usize, value: u64) {
    unsafe { *table.add(index & 0x1FF) = value; }
}

/// True if every one of the 512 entries in the table at physical `frame` is
/// non-present (i.e. the table holds no leaf and points to no deeper level).
unsafe fn table_is_empty(frame: u64) -> bool {
    let table = pte_deref(frame);
    for i in 0..512 {
        if unsafe { read_pte(table, i) } & PTE_PRESENT != 0 {
            return false;
        }
    }
    true
}

/// Free 4-level intermediate page tables that became empty after the leaf at
/// `vaddr` was unmapped, returning their frames to `alloc`.
///
/// Walks from the PML4 down to the deepest (Level-1) table holding the leaf.
/// After the leaf is cleared (already done by the caller), each table is freed
/// and its parent entry cleared only when it has no remaining present entries.
/// The root (PML4) itself is never freed — it is owned by the kernel.
fn reclaim_empty_tables(root: u64, alloc: &mut BitmapAllocator, vaddr: u64) {
    // Level-0 (leaf) table index, Level-1 (PD), Level-2 (PDPT), Level-3 (PML4).
    #[rustfmt::skip]
    let (i_pt, i_pd, i_pdpt, i_pml4) = (
        ((vaddr >> 12) & 0x1FF) as usize,
        ((vaddr >> 21) & 0x1FF) as usize,
        ((vaddr >> 30) & 0x1FF) as usize,
        ((vaddr >> 39) & 0x1FF) as usize,
    );

    unsafe {
        let pml4 = pte_deref(root);
        let pml4e = read_pte(pml4, i_pml4);
        if pml4e & PTE_PRESENT == 0 {
            return;
        }
        let pdpt = pte_deref(pte_frame(pml4e));
        let pdpte = read_pte(pdpt, i_pdpt);
        if pdpte & PTE_PRESENT == 0 {
            return;
        }
        let pd = pte_deref(pte_frame(pdpte));
        let pde = read_pte(pd, i_pd);
        if pde & PTE_PRESENT == 0 {
            return;
        }
        // This must be a non-leaf entry pointing at the Level-1 table if the
        // map was created with 2M huge pages this entry is a leaf — nothing to
        // reclaim (the unmapped page was a 4K leaf deeper down).
        let pt_frame = pte_frame(pde);
        let pt = pte_deref(pt_frame);
        if read_pte(pt, i_pt) & PTE_PRESENT != 0 {
            return; // line already re-mapped; leave as-is
        }

        if table_is_empty(pt_frame) {
            alloc.free(pt_frame);
            write_pte(pd, i_pd, 0);
            // PD may now be empty → free it and clear PDPT entry.
            let pd_frame = pte_frame(pdpte);
            if table_is_empty(pd_frame) {
                alloc.free(pd_frame);
                write_pte(pdpt, i_pdpt, 0);
                let pdpt_frame = pte_frame(pml4e);
                if table_is_empty(pdpt_frame) {
                    alloc.free(pdpt_frame);
                    write_pte(pml4, i_pml4, 0);
                }
            }
        }
    }
}

pub fn translate(root: u64, vaddr: u64) -> Option<u64> {
    let mapper = mapper_at(root);
    mapper.translate_addr(VirtAddr::new(vaddr)).map(|p| p.as_u64())
}

/// Remove the WRITABLE flag from a single 4 KiB page, making it read-only
/// in both the identity and higher-half mappings.
///
/// The page must already be mapped with 4 KiB granularity via 4-level paging
/// (the kernel identity map uses 4 KiB pages for kernel-image pages).
/// Panics if the page is not present.
pub fn make_read_only_both(root: u64, vaddr: u64) {
    let set_ro = |addr: u64| {
        let mut mapper = mapper_at(root);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        // Reconstruct flags matching what leaf_flags() would have set for
        // .data/.bss (READ | WRITE | NO_EXECUTE) minus WRITABLE.
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::ACCESSED
            | PageTableFlags::NO_EXECUTE;
        unsafe {
            mapper
                .update_flags(page, flags)
                .expect("make_read_only: update_flags failed")
                .flush();
        }
    };
    set_ro(vaddr);
    set_ro(crate::mm::vmm::KERNEL_VMA_BASE + vaddr);
}

/// Switch to the given root table (physical address of the PML4).
///
/// # Safety
/// The caller must ensure the new page table maps the current instruction
/// stream and stack.
pub unsafe fn activate(root: u64) {
    let frame = PhysFrame::containing_address(XPhysAddr::new(root));
    unsafe { Cr3::write(frame, Cr3Flags::empty()); }
}
