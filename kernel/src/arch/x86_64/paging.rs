use x86_64::registers::control::{Cr0, Cr0Flags};
use x86_64::registers::model_specific::{Efer, EferFlags, Msr};

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm, KERNEL_VMA_BASE};
use crate::KernelLayout;

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

const TRAMPOLINE_PHYS: u64 = 0x8000;
const IA32_APIC_BASE_MSR: u32 = 0x1B;

/// Build identity-mapped page tables together with a higher-half alias of
/// the kernel image at `KERNEL_VMA_BASE + phys_addr`.
///
/// Returns a `Vmm` that the caller can activate (via `vmm::activate`).
///
/// NXE (No-Execute) and WP (Write-Protect) are enabled here so that the
/// `NO_EXECUTE` page-table bit and the W^X policy are effective the moment
/// the new tables are loaded.
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
    framebuffer_bpp: u8,
) -> Vmm {
    let fb_size = (framebuffer_stride * framebuffer_height * framebuffer_bpp as usize) as u64;
    let fb_start = framebuffer_addr;
    let fb_end = framebuffer_addr.saturating_add(fb_size);

    // ── Enable NXE + WP ────────────────────────────────────────────
    unsafe {
        Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE));
        Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT));
    }

    let mut vmm = Vmm::new(allocator);
    let guard_page = stack_guard & !(PAGE_4K - 1);

    // Read the local APIC base so it can be mapped uncacheable and so
    // we know the minimum extent of the identity-map range.
    let apic_base_msr = Msr::new(IA32_APIC_BASE_MSR);
    let apic_base = unsafe { apic_base_msr.read() } & !(PAGE_4K - 1);

    // Identity-map at least up to the end of physical RAM and the local
    // APIC MMIO region.  The framebuffer (which may be far above RAM on
    // 64-bit systems) is handled separately below.
    let ram_end = (allocator.alloc_end().max(apic_base + PAGE_4K) + PAGE_2M - 1) & !(PAGE_2M - 1);

    // ── Identity-map all RAM up to ram_end ─────────────────────────
    let mut chunk = 0u64;
    while chunk < ram_end {
        let chunk_end = chunk + PAGE_2M;

        let overlaps_kernel = chunk < layout.kernel_end && chunk_end > layout.kernel_start;
        let contains_guard = stack_guard != 0 && guard_page >= chunk && guard_page < chunk_end;
        let is_first = chunk == 0;
        let contains_apic = chunk <= apic_base && chunk_end > apic_base;

        if overlaps_kernel || contains_guard || is_first || contains_apic {
            let mut page_addr = chunk;
            while page_addr < chunk_end {
                if page_addr == 0 || (stack_guard != 0 && page_addr == guard_page) {
                    page_addr += PAGE_4K;
                    continue;
                }
                let mut flags = leaf_flags(page_addr, layout, fb_start, fb_end);
                if page_addr == TRAMPOLINE_PHYS {
                    flags |= PageFlags::EXECUTE;
                }
                if page_addr == apic_base {
                    flags |= PageFlags::NO_CACHE;
                }
                vmm.map_4k(allocator, page_addr, page_addr, flags);
                page_addr += PAGE_4K;
            }
        } else {
            let mut flags = PageFlags::READ | PageFlags::WRITE;
            if chunk < fb_end && chunk_end > fb_start {
                flags |= PageFlags::NO_CACHE;
            }
            vmm.map_2m(allocator, chunk, chunk, flags);
        }

        chunk = chunk_end;
    }

    // ── Identity-map framebuffer extension beyond RAM ──────────────
    if fb_end > ram_end {
        let fb_map_start = (fb_start & !(PAGE_2M - 1)).max(ram_end);
        let fb_map_end = (fb_end + PAGE_2M - 1) & !(PAGE_2M - 1);
        chunk = fb_map_start;
        while chunk < fb_map_end {
            vmm.map_2m(
                allocator,
                chunk,
                chunk,
                PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
            );
            chunk += PAGE_2M;
        }
    }

    // ── Higher-half kernel alias ───────────────────────────────────
    // Every 4 KiB page of the kernel image is also mapped at
    // KERNEL_VMA_BASE + phys_addr with identical permissions.
    // Enforce the CR3 handoff invariant: all linked kernel pages (including
    // .data, .bss, and the bootstrap stack) remain identity-mapped.
    let kernel_map_start = layout.kernel_start & !(PAGE_4K - 1);
    let kernel_map_end = (layout.kernel_end + PAGE_4K - 1) & !(PAGE_4K - 1);
    let mut addr = kernel_map_start;
    while addr < kernel_map_end {
        if vmm.translate(addr).is_none() {
            vmm.map_4k(allocator, addr, addr, leaf_flags(addr, layout, fb_start, fb_end));
        }
        addr += PAGE_4K;
    }

    let mut addr = layout.kernel_start;
    while addr < layout.kernel_end {
        let flags = leaf_flags(addr, layout, fb_start, fb_end);
        vmm.map_4k(allocator, KERNEL_VMA_BASE + addr, addr, flags);
        addr += PAGE_4K;
    }

    vmm
}

/// Per-page permissions based on the section a physical address falls in.
fn leaf_flags(addr: u64, layout: &KernelLayout, fb_start: u64, fb_end: u64) -> PageFlags {
    let mut flags = if addr >= layout.text_start && addr < layout.text_end {
        PageFlags::READ | PageFlags::EXECUTE
    } else if addr >= layout.rela_dyn_start && addr < layout.rela_dyn_end {
        PageFlags::READ
    } else if addr >= layout.rodata_start && addr < layout.rodata_end {
        PageFlags::READ
    } else {
        PageFlags::READ | PageFlags::WRITE
    };
    if addr >= fb_start && addr < fb_end {
        flags |= PageFlags::NO_CACHE;
    }
    flags
}
