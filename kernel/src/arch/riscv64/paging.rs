use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm, KERNEL_VMA_BASE};
use crate::KernelLayout;

const PAGE_SIZE: u64 = 4096;
const PAGE_2M: u64 = 512 * 4096;

/// Build identity-mapped page tables together with a higher-half alias of
/// the kernel image at `KERNEL_VMA_BASE + phys_addr`.
///
/// Returns a `Vmm` that the caller can activate (via `vmm::activate`).
///
/// # Safety
/// - `allocator` is initialised and has free frames.
pub fn setup(
    allocator: &mut BitmapAllocator,
    layout: &KernelLayout,
    stack_guard: u64,
    framebuffer_addr: u64,
    framebuffer_height: usize,
    framebuffer_stride: usize,
) -> Vmm {
    let fb_size = (framebuffer_stride * framebuffer_height * 4) as u64;
    let fb_start = framebuffer_addr;
    let fb_end = framebuffer_addr.saturating_add(fb_size);

    let min_end = 4u64 * 1024 * 1024 * 1024;
    let max_addr = fb_end.max(min_end).max(allocator.alloc_end());
    let max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let mut vmm = Vmm::new(allocator);

    // ── Identity-map all RAM ───────────────────────────────────────
    let mut chunk = 0u64;
    while chunk < max_addr {
        let chunk_end = chunk + PAGE_2M;

        let overlaps_kernel = chunk < layout.kernel_end && chunk_end > layout.kernel_start;
        let contains_guard = stack_guard != 0 && stack_guard >= chunk && stack_guard < chunk_end;
        let is_first = chunk == 0;

        if overlaps_kernel || contains_guard || is_first {
            let mut page_addr = chunk;
            while page_addr < chunk_end {
                if page_addr == 0 || (stack_guard != 0 && page_addr == stack_guard) {
                    page_addr += PAGE_SIZE;
                    continue;
                }
                let flags = leaf_flags(page_addr, layout, fb_start, fb_end);
                vmm.map_4k(allocator, page_addr, page_addr, flags);
                page_addr += PAGE_SIZE;
            }
        } else {
            vmm.map_2m(allocator, chunk, chunk, PageFlags::READ | PageFlags::WRITE);
        }

        chunk = chunk_end;
    }

    // ── Higher-half kernel alias ───────────────────────────────────
    let mut addr = layout.kernel_start;
    while addr < layout.kernel_end {
        let flags = leaf_flags(addr, layout, fb_start, fb_end);
        vmm.map_4k(allocator, KERNEL_VMA_BASE + addr, addr, flags);
        addr += PAGE_SIZE;
    }

    vmm
}

/// Per-page permissions based on the section a physical address falls in.
fn leaf_flags(addr: u64, layout: &KernelLayout, fb_start: u64, fb_end: u64) -> PageFlags {
    let mut flags = if addr >= layout.text_start && addr < layout.text_end {
        PageFlags::READ | PageFlags::EXECUTE
    } else if addr >= layout.rodata_start && addr < layout.rodata_end {
        PageFlags::READ
    } else {
        PageFlags::READ | PageFlags::WRITE
    };
    if addr >= fb_start && addr < fb_end {
        flags &= !PageFlags::EXECUTE;
    } else if !(addr >= layout.text_start && addr < layout.text_end) {
        flags &= !PageFlags::EXECUTE;
    }
    flags
}
