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
/// (DIRECT_MAP) is built at `PHYS_MAP_BASE` covering exactly the *usable*
/// RAM spans — MMIO/ACPI holes below `alloc_end` get no alias, and the
/// stack-guard frame is excluded from its span.
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

    // ── Enable SMEP + SMAP + PKE (BSP) — runtime-gated, no-op when unsupported ──
    // SMAP is safe here: every user-VA deref runs inside syscall_dispatch's
    // UserAccess (stac/clac) guard. PKE must be on before any task applies a
    // non-zero PKRU.
    crate::arch::x86_64::cpufeat::enable_smep();
    crate::arch::x86_64::cpufeat::enable_smap();
    crate::arch::x86_64::cpufeat::enable_pke();

    let mut vmm = Vmm::new(allocator);
    let guard_page = stack_guard & !(PAGE_4K - 1);

    // Read the local APIC base so it can be mapped uncacheable.
    let apic_base_msr = Msr::new(IA32_APIC_BASE_MSR);
    let apic_base = unsafe { apic_base_msr.read() } & !(PAGE_4K - 1);

    // ── Kernel image at KERNEL_VMA - kaslr — the primary map ─────────
    // True KASLR: image is remapped at randomized VMA `KASLR_BASE`.
    // Phys backing stays at `KERNEL_LMA_BASE`; `paddr = vaddr - KASLR_BASE + LMA`.
    // `.text` RX, etc. via `leaf_flags` against the randomized addresses.
    let kaslr = crate::mm::layout::kaslr_offset();
    let kaslr_base = KERNEL_VMA_BASE.wrapping_sub(kaslr);
    // Adjust layout view for permission lookup.
    let adj_layout = crate::KernelLayout {
        kernel_start: layout.kernel_start.wrapping_sub(kaslr),
        kernel_end: layout.kernel_end.wrapping_sub(kaslr),
        text_start: layout.text_start.wrapping_sub(kaslr),
        text_end: layout.text_end.wrapping_sub(kaslr),
        rela_dyn_start: layout.rela_dyn_start.wrapping_sub(kaslr),
        rela_dyn_end: layout.rela_dyn_end.wrapping_sub(kaslr),
        rodata_start: layout.rodata_start.wrapping_sub(kaslr),
        rodata_end: layout.rodata_end.wrapping_sub(kaslr),
        #[cfg(target_arch = "x86_64")]
        idt_start: layout.idt_start.wrapping_sub(kaslr),
        #[cfg(target_arch = "x86_64")]
        idt_end: layout.idt_end.wrapping_sub(kaslr),
    };
    let kernel_start = adj_layout.kernel_start & !(PAGE_4K - 1);
    let kernel_end = (adj_layout.kernel_end + PAGE_4K - 1) & !(PAGE_4K - 1);
    let mut vaddr = kernel_start;
    while vaddr < kernel_end {
        let paddr = vaddr.wrapping_sub(kaslr_base).wrapping_add(KERNEL_LMA_BASE);
        vmm.map_4k(
            allocator,
            vaddr,
            paddr,
            leaf_flags(vaddr, &adj_layout, fb_start, fb_end),
        );
        vaddr += PAGE_4K;
    }
    // Keep fixed-VMA alias for the running instruction stream after CR3 switch.
    // New tables would otherwise unmap the current RIP (`KERNEL_VMA_BASE + off`)
    // while we are still executing there via the boot tables. Duplicate maps
    // to the same LMA, no extra frames — both aliases alias. Always map when
    // slid (overlap cannot happen with 256M min offset and 256M image, but keep
    // alias unconditionally for correctness if constants change).
    if kaslr != 0 {
        let orig_start = layout.kernel_start & !(PAGE_4K - 1);
        let orig_end = (layout.kernel_end + PAGE_4K - 1) & !(PAGE_4K - 1);
        let mut v2 = orig_start;
        while v2 < orig_end {
            let paddr2 = v2.wrapping_sub(KERNEL_VMA_BASE).wrapping_add(KERNEL_LMA_BASE);
            vmm.map_4k(allocator, v2, paddr2, leaf_flags(v2, layout, fb_start, fb_end));
            v2 += PAGE_4K;
        }
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
    // Map the *usable* RAM spans once at PHYS_MAP_BASE with READ|WRITE so the
    // VMM walkers can deref page-table frames through the physmap instead of
    // the identity map once `init_physmap` is called after activation.
    // Built here (still using the firmware identity map) because it must be
    // present in the *live* kernel root before any runtime walk.  The stack
    // guard frame is left unmapped so the physmap never exposes it.
    //
    // Only `usable_regions()` spans are mapped — PCI MMIO / ACPI / framebuffer
    // holes below `alloc_end` get NO writable alias, so a stray kernel pointer
    // into a hole faults instead of silently scribbling on a device.
    let mut spans: [(u64, u64); crate::mm::phys_alloc::MAX_USABLE_REGIONS] =
        [(0, 0); crate::mm::phys_alloc::MAX_USABLE_REGIONS];
    let mut span_count = 0usize;
    for r in allocator.usable_regions() {
        assert!(
            span_count < spans.len(),
            "physmap: more usable regions than tracked"
        );
        spans[span_count] = *r;
        span_count += 1;
    }

    // 4 KiB-maps [start, end) at PHYS_MAP_BASE, skipping the stack guard.
    fn map_span_4k(
        vmm: &mut Vmm,
        allocator: &mut BitmapAllocator,
        start: u64,
        end: u64,
        guard_page: u64,
        has_guard: bool,
    ) {
        let mut page = start;
        while page < end {
            if !has_guard || page != guard_page {
                vmm.map_4k(
                    allocator,
                    PHYS_MAP_BASE + page,
                    page,
                    PageFlags::READ | PageFlags::WRITE,
                );
            }
            page += PAGE_4K;
        }
    }

    for i in 0..span_count {
        let (r_base, r_size) = spans[i];
        let start = r_base & !(PAGE_4K - 1);
        let end_raw = r_base.saturating_add(r_size);
        if end_raw <= start {
            continue;
        }
        // Round the exclusive end up to a 4 KiB boundary without overflow.
        let Some(end) = (end_raw - 1)
            .checked_add(PAGE_4K - 1)
            .map(|v| v & !(PAGE_4K - 1))
        else {
            continue;
        };

        // Interior 2 MiB-aligned body; head/tail stay 4 KiB so region edges
        // never spill into adjacent holes.
        let body_start = (start + PAGE_2M - 1) & !(PAGE_2M - 1);
        let body_end = end & !(PAGE_2M - 1);
        let head_end = body_start.min(end);

        if start < head_end {
            map_span_4k(&mut vmm, allocator, start, head_end, guard_page, stack_guard != 0);
        }

        let mut frame = body_start.max(start);
        while frame + PAGE_2M <= body_end {
            if stack_guard != 0 && frame <= guard_page && guard_page < frame + PAGE_2M {
                // Guard falls inside this chunk — split into 4 KiB pages.
                map_span_4k(&mut vmm, allocator, frame, frame + PAGE_2M, guard_page, true);
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

        let tail_start = body_end.max(head_end);
        if tail_start < end {
            map_span_4k(&mut vmm, allocator, tail_start, end, guard_page, stack_guard != 0);
        }
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
