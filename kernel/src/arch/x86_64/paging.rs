use x86_64::registers::control::{Cr0, Cr0Flags};
use x86_64::registers::model_specific::{Efer, EferFlags, Msr};

use crate::KernelLayout;
use crate::mm::layout::{FB_VADDR_BASE, LAPIC_VADDR_BASE, PHYS_MAP_BASE};
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{KERNEL_VMA_BASE, PageFlags, Vmm, init_pat_wc};

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

const TRAMPOLINE_PHYS: u64 = 0x8000;
/// AP stack page for the trampoline's real/protected-mode tail.  `_trampoline_start`
/// sets `sp = 0x7000` and uses it for the `push 0x18; push eax; retf` far-return
/// into long mode; pushes grow *down* from 0x7000, so the frame that actually
/// receives the writes is page `0x6000`.  (The per-AP record `stack_top` is a
/// high kernel stack, switched to at `_trampoline_lm`.)
const TRAMPOLINE_STACK_PHYS: u64 = 0x6000;
const IA32_APIC_BASE_MSR: u32 = 0x1B;

/// Physical load base of the kernel image.
///
/// Phase 2 pinned the kernel-region LMA to `__low_end` (`linker.ld`), so the
/// higher-half VMAs `[KERNEL_VMA_BASE, kernel_end)` load at physical
/// `[KERNEL_LMA_BASE, ...)`.  Every kernel page uses the derived mapping
/// `phys = vaddr - KERNEL_VMA_BASE + KERNEL_LMA_BASE`.
/// Phase 5: replace with the `__kernel_start_phys` linker symbol (`== __low_end`
/// today), kept in sync with `linker.ld`.
const KERNEL_LMA_BASE: u64 = 0x600000;

/// Build the higher-half-PRIMARY page tables.
///
/// The kernel image is mapped *once*, at `KERNEL_VMA` (4 KiB per page, W^X
/// via `leaf_flags`) — there is no low identity alias any more.  The only
/// low identity mappings kept are the live windows:
///   - the SMP trampoline region `[0x8000, 0x8E00)`,
///   - the local APIC MMIO (uncacheable),
///   - the framebuffer's physical range (write-combining — the graphics
///     driver derefs the physical address as its VA).
/// Everything else in low memory is unmapped.  The private physmap
/// (DIRECT_MAP) is built at `PHYS_MAP_BASE` covering physical
/// `[0, alloc_end)` with the stack-guard frame excluded.
///
/// The kernel runs on the high `.stack` (the top of the kernel image, set by
/// the boot stubs at `__stack_end`), NOT on any low stack, so driver domains
/// can legitimately have empty low halves — there is nothing live down there.
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

    // ── Program PAT so entry 1 = Write-Combining ───────────────────
    init_pat_wc();

    // ── Enable NXE + WP ────────────────────────────────────────────
    unsafe {
        Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE));
        Cr0::update(|f| f.insert(Cr0Flags::WRITE_PROTECT));
    }

    let mut vmm = Vmm::new(allocator);
    let guard_page = stack_guard & !(PAGE_4K - 1);

    // Read the local APIC base so it can be mapped uncacheable.
    let apic_base_msr = Msr::new(IA32_APIC_BASE_MSR);
    let apic_base = unsafe { apic_base_msr.read() } & !(PAGE_4K - 1);

    // ── Kernel image at KERNEL_VMA — the primary map ────────────────
    // Map every 4 KiB page of `[KERNEL_VMA, kernel_end)` to its physical
    // backing frame via `phys = vaddr - KERNEL_VMA + KERNEL_LMA_BASE`.
    // `.text` is RX, `.rela_dyn`/`.rodata` R, and `.data`/`.bss`/`.idt`/
    // `.stack` RW+NX (see `leaf_flags`).  The currently-executing kernel
    // (instruction stream AND the high `.stack` at the top of the image,
    // which is < `kernel_end`) is covered by this mapping.
    let kernel_start = layout.kernel_start & !(PAGE_4K - 1);
    let kernel_end = (layout.kernel_end + PAGE_4K - 1) & !(PAGE_4K - 1);
    let mut vaddr = kernel_start;
    while vaddr < kernel_end {
        let paddr = vaddr - KERNEL_VMA_BASE + KERNEL_LMA_BASE;
        vmm.map_4k(
            allocator,
            vaddr,
            paddr,
            leaf_flags(vaddr, layout, fb_start, fb_end),
        );
        vaddr += PAGE_4K;
    }

    // ── Minimal identity windows ────────────────────────────────────

    // (a) SMP trampoline — real-mode/protected-mode AP code runs at these low
    // addresses, and the 64-bit trampoline tail (`lock xadd` / record reads)
    // also executes here before long-jumping to `ap_entry64`.  The shared
    // data (TrampolineData at 0x8700, boot counter at 0x8CF8, per-AP records
    // at 0x8D00) all live inside the same 4 KiB frame, so the one page is RWX.
    if stack_guard == 0 || guard_page != TRAMPOLINE_PHYS {
        vmm.map_4k(
            allocator,
            TRAMPOLINE_PHYS,
            TRAMPOLINE_PHYS,
            PageFlags::READ | PageFlags::WRITE | PageFlags::EXECUTE,
        );
    }

    // (a2) AP stack page for the trampoline's 32-bit tail — the far-return
    // pushes (`push 0x18; push eax`) land at 0x6ffc/0x6ff8, i.e. in the page
    // below the 0x7000 stack top.  RW only.
    if stack_guard == 0 || guard_page != TRAMPOLINE_STACK_PHYS {
        vmm.map_4k(
            allocator,
            TRAMPOLINE_STACK_PHYS,
            TRAMPOLINE_STACK_PHYS,
            PageFlags::READ | PageFlags::WRITE,
        );
    }

    // (b) Local APIC MMIO — identity, uncacheable (used constantly after
    // `init_apic`).
    if apic_base != 0 {
        vmm.map_4k(
            allocator,
            apic_base,
            apic_base,
            PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
        );
    }

    // (b2) Local APIC MMIO — higher-half device window.  Same page as the
    // identity window above, re-mapped at `LAPIC_VADDR_BASE` so user task roots
    // (which clone the higher half) can reach the APIC: `apic_eoi` runs on the
    // process CR3 when a device IRQ fires during a syscall.  Uncached like the
    // identity window.
    if apic_base != 0 {
        vmm.map_4k(
            allocator,
            LAPIC_VADDR_BASE,
            apic_base,
            PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
        );
    }

    // (d) Framebuffer — identity, write-combining.  The graphics driver
    // derefs the framebuffer's physical address as its VA, so the physical
    // range must be mapped at its own address.  May be far above 4 GiB.
    // 4 KiB pages keep the window tight (no over-mapping of adjacent RAM)
    // and avoid aliasing the other low windows.
    let fb_map_start = fb_start & !(PAGE_4K - 1);
    let fb_map_end = (fb_end + PAGE_4K - 1) & !(PAGE_4K - 1);
    let mut page = fb_map_start;
    while page < fb_map_end {
        let already_mapped = page == apic_base
            || (page >= TRAMPOLINE_PHYS && page < TRAMPOLINE_PHYS + PAGE_4K)
            || (page >= TRAMPOLINE_STACK_PHYS && page < TRAMPOLINE_STACK_PHYS + PAGE_4K)
            || (stack_guard != 0 && page == guard_page);
        if !already_mapped {
            vmm.map_4k(
                allocator,
                page,
                page,
                PageFlags::READ | PageFlags::WRITE | PageFlags::WRITE_COMBINING,
            );
        }
        page += PAGE_4K;
    }

    // (d2) Framebuffer — higher-half device window.  Same physical pages as the
    // identity window above, re-mapped at `FB_VADDR_BASE` so user task roots
    // (which clone the higher half) can reach the scanout buffer: `/dev/fb`
    // write-through happens on the process CR3 during a syscall.  WC like the
    // identity window; NX is automatic (page_flags_to_x86 sets NO_EXECUTE
    // whenever EXECUTE is absent).
    let mut page = fb_map_start;
    while page < fb_map_end {
        let already_mapped = page == apic_base
            || (page >= TRAMPOLINE_PHYS && page < TRAMPOLINE_PHYS + PAGE_4K)
            || (page >= TRAMPOLINE_STACK_PHYS && page < TRAMPOLINE_STACK_PHYS + PAGE_4K)
            || (stack_guard != 0 && page == guard_page);
        if !already_mapped {
            vmm.map_4k(
                allocator,
                FB_VADDR_BASE + (page - fb_map_start),
                page,
                PageFlags::READ | PageFlags::WRITE | PageFlags::WRITE_COMBINING,
            );
        }
        page += PAGE_4K;
    }

    // ── DIRECT_MAP (private physmap) ───────────────────────────────
    // Map physical `[0, dm_end)` once at PHYS_MAP_BASE with READ|WRITE so the
    // VMM walkers can deref page-table frames through the physmap instead of
    // the identity map once `init_physmap` is called after activation.
    // Built here (still using the firmware identity map) because it must be
    // present in the *live* kernel root before any runtime walk.  The stack
    // guard frame is left unmapped so the physmap never exposes it.
    let dm_end = (allocator.alloc_end() + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut frame = 0u64;
    while frame + PAGE_2M <= dm_end {
        if stack_guard != 0 && frame <= guard_page && guard_page < frame + PAGE_2M {
            // The guard falls inside this 2 MiB chunk — split it into 4 KiB
            // pages, skipping the guard frame.
            let mut page = frame;
            while page < frame + PAGE_2M {
                if page != guard_page {
                    vmm.map_4k(
                        allocator,
                        PHYS_MAP_BASE + page,
                        page,
                        PageFlags::READ | PageFlags::WRITE,
                    );
                }
                page += PAGE_4K;
            }
        } else {
            vmm.map_2m(
                allocator,
                PHYS_MAP_BASE + frame,
                frame,
                PageFlags::READ | PageFlags::WRITE,
            );
        }
        frame += PAGE_2M;
    }
    while frame < dm_end {
        if stack_guard == 0 || frame != guard_page {
            vmm.map_4k(
                allocator,
                PHYS_MAP_BASE + frame,
                frame,
                PageFlags::READ | PageFlags::WRITE,
            );
        }
        frame += PAGE_4K;
    }

    vmm
}

/// Per-page permissions based on the kernel section a *virtual* address falls
/// in.  `layout.*` bounds are kernel-region VMAs (the kernel now links high),
/// so `addr` must be the VMA being mapped, not a physical address.
///
/// `.text` is executable; everything else is NX by default (the arch PTE
/// builder sets NO_EXECUTE whenever EXECUTE is absent).  `WRITE_COMBINING` is
/// added for framebuffer pages (only reachable through the identity fb window,
/// never through the kernel image).
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
        flags |= PageFlags::WRITE_COMBINING;
    }
    flags
}
