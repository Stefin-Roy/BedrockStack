//! x86_64 4-level page table operations.
//!
//! Wraps the `x86_64` crate's `OffsetPageTable` with identity offset
//! (virtual == physical) so we can reuse its robust page-table walker.

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
    let root_ptr = root as *mut PageTable;
    unsafe { OffsetPageTable::new(&mut *root_ptr, VirtAddr::new(0)) }
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
        f |= PageTableFlags::NO_CACHE;
    }
    if flags.contains(PageFlags::USER) {
        f |= PageTableFlags::USER_ACCESSIBLE;
    }
    f
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

pub fn unmap_4k(root: u64, _alloc: &mut BitmapAllocator, vaddr: u64) -> bool {
    let mut mapper = mapper_at(root);

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    match mapper.unmap(page) {
        Ok((_freed_frame, flush)) => {
            flush.flush();
            // TODO: free `_freed_frame` back when it's an intermediate table frame.
            true
        }
        Err(_) => false,
    }
}

pub fn translate(root: u64, vaddr: u64) -> Option<u64> {
    let mapper = mapper_at(root);
    mapper.translate_addr(VirtAddr::new(vaddr)).map(|p| p.as_u64())
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
