//! Virtual memory setup — identity mapping in a freshly built page table.
//!
//! We do NOT reuse the firmware's page tables. Reusing them meant huge-page
//! mappings created by UEFI silently blocked our 4 KiB (re)mappings — in
//! particular the framebuffer's `NO_CACHE` flag could be dropped. Instead we
//! build a brand-new PML4 that we fully control, then load CR3 once.
//!
//! Design:
//! - The bulk of RAM is identity-mapped with 2 MiB huge pages. This is orders
//!   of magnitude faster than per-4 KiB mapping and uses far less memory for
//!   the page tables themselves.
//! - Default leaf permissions are `WRITABLE | NO_EXECUTE` (W^X for data).
//! - The framebuffer's pages get `NO_CACHE`.
//! - The kernel image is mapped with 4 KiB pages so per-section permissions can
//!   be applied: `.text` is executable + read-only, `.rodata` is read-only +
//!   non-executable, everything else is writable + non-executable.
//! - The NULL page (frame 0) and the stack guard page are left unmapped so
//!   null derefs and stack overflows fault instead of corrupting memory.

use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags};
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size2MiB,
    Size4KiB,
};
use x86_64::{PhysAddr as XPhysAddr, VirtAddr};

use crate::mm::phys_alloc::BitmapAllocator;
use crate::KernelLayout;

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

/// Adapter that implements `FrameAllocator<Size4KiB>` for `BitmapAllocator`.
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

/// Build a fresh identity map and switch to it.
///
/// # Safety
/// - `allocator` is valid and initialized.
/// - `framebuffer_addr/height/stride` describe a valid framebuffer.
/// - `stack_guard` is the physical address of the stack guard page.
/// - After the CR3 switch the current RIP/RSP must still be identity-mapped,
///   which holds because we map all RAM (including the kernel and stack).
pub fn setup(
    allocator: &mut BitmapAllocator,
    layout: &KernelLayout,
    stack_guard: u64,
    framebuffer_addr: u64,
    framebuffer_height: usize,
    framebuffer_stride: usize,
) {
    // stride is pixels-per-scanline, so bytes-per-row = stride * 4.
    let fb_size = (framebuffer_stride * framebuffer_height * 4) as u64;
    let fb_start = framebuffer_addr;
    let fb_end = framebuffer_addr.saturating_add(fb_size);

    // Map at least the first 4 GiB (kernel, framebuffer, hardware MMIO), the
    // whole framebuffer, and all real RAM the physical allocator can serve.
    // We bound by `alloc_end()` (the actual physical-RAM extent), NOT the address
    // space end: OVMF/QEMU report gigantic "conventional" regions in high address
    // space that are not backed by real RAM, and trying to map them would make us
    // fabricate page tables for nonexistent memory.
    let min_end = 4u64 * 1024 * 1024 * 1024;
    let max_addr = fb_end.max(min_end).max(allocator.alloc_end());
    let max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    // Enable NXE so the NO_EXECUTE bit is valid, and WP so supervisor writes to
    // read-only pages fault (required for W^X to actually protect .text).
    unsafe {
        Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE));
        Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT));
    }

    // Allocate and zero a fresh PML4.
    let pml4_phys = allocator.alloc().expect("out of memory for PML4");
    let pml4 = pml4_phys as *mut PageTable;
    unsafe {
        (*pml4).zero();
    }
    let mut mapper = unsafe { OffsetPageTable::new(&mut *pml4, VirtAddr::new(0)) };
    let mut frame_allocator = BitmapFrameAllocator { inner: allocator };

    let guard_page = stack_guard & !(PAGE_4K - 1);

    let mut chunk = 0u64;
    while chunk < max_addr {
        let chunk_end = chunk + PAGE_2M;

        let overlaps_kernel = chunk < layout.kernel_end && chunk_end > layout.kernel_start;
        let contains_guard = stack_guard != 0 && guard_page >= chunk && guard_page < chunk_end;
        let is_first = chunk == 0;

        if overlaps_kernel || contains_guard || is_first {
            // Fine-grained: 4 KiB pages so we can skip NULL/guard and apply W^X.
            let mut page_addr = chunk;
            while page_addr < chunk_end {
                if page_addr == 0 || (stack_guard != 0 && page_addr == guard_page) {
                    page_addr += PAGE_4K;
                    continue;
                }
                let flags = leaf_flags_4k(page_addr, layout, fb_start, fb_end);
                let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_addr));
                let frame = PhysFrame::<Size4KiB>::containing_address(XPhysAddr::new(page_addr));
                unsafe {
                    mapper
                        .map_to_with_table_flags(
                            page,
                            frame,
                            flags,
                            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                            &mut frame_allocator,
                        )
                        .expect("4KiB map failed")
                        .ignore(); // final CR3 load flushes the whole TLB
                }
                page_addr += PAGE_4K;
            }
        } else {
            // Bulk RAM: 2 MiB huge page, writable + non-executable.
            let mut flags = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_EXECUTE;
            if chunk < fb_end && chunk_end > fb_start {
                flags |= PageTableFlags::NO_CACHE;
            }
            let page = Page::<Size2MiB>::containing_address(VirtAddr::new(chunk));
            let frame = PhysFrame::<Size2MiB>::containing_address(XPhysAddr::new(chunk));
            unsafe {
                mapper
                    .map_to_with_table_flags(
                        page,
                        frame,
                        flags,
                        PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                        &mut frame_allocator,
                    )
                    .expect("2MiB map failed")
                    .ignore();
            }
        }

        chunk = chunk_end;
    }

    // Switch to the new page table (this also flushes the entire TLB).
    unsafe {
        let frame = PhysFrame::containing_address(XPhysAddr::new(pml4_phys));
        Cr3::write(frame, Cr3Flags::empty());
    }
}

/// Compute leaf permissions for a 4 KiB page inside the kernel image (or the
/// NULL/guard chunk). Applies W^X per section and `NO_CACHE` for framebuffer.
fn leaf_flags_4k(addr: u64, layout: &KernelLayout, fb_start: u64, fb_end: u64) -> PageTableFlags {
    let mut flags = if addr >= layout.text_start && addr < layout.text_end {
        // .text: executable, read-only.
        PageTableFlags::PRESENT
    } else if addr >= layout.rela_dyn_start && addr < layout.rela_dyn_end {
        // .rela.dyn: read-only, non-executable (relocation data).
        PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE
    } else if addr >= layout.rodata_start && addr < layout.rodata_end {
        // .rodata: read-only, non-executable.
        PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE
    } else {
        // .data/.bss/stack/other: writable, non-executable.
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE
    };
    if addr >= fb_start && addr < fb_end {
        flags |= PageTableFlags::NO_CACHE;
    }
    flags
}
