//! Central kernel virtual-address layout.
//!
//! Every non-identity VA window lives in a small number of explicitly-sized,
//! non-overlapping regions carved above `KERNEL_VMA_BASE`.  Regions allocate
//! *downward* from their `top`, so each region's upper bound is its base and
//! its lower bound is a floor; a region that would be carved into its lower
//! neighbour panics.
//!
//! The physical identity map is intentionally *not* part of this layout: it is
//! a boot-time transition window (kernel image + low-memory trampoline) and is
//! contacted only by `mm`/`arch`/`smp` internals through the private physmap
//! region (see `mm::physmap`).  Consumers (drivers, VFS, graphics) never touch
//! physical addresses; they get virtual addresses from `VirtualMemoryManager`
//! or `DmaAllocator`.

use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::filesystems::vfs::irq::IrqMutex;

///
/// Start of the canonical higher-half region (also Start of x86_64's standard
/// negative-address kernel range and Sv39's sign-extended high half).
pub const KERNEL_VMA_BASE: u64 = 0xFFFFFFFF_80000000;

/// Size reserved for the higher-half kernel image (Phase E target).
pub const KERNEL_IMAGE_SIZE: u64 = 0x1000_0000; // 256 MiB
pub const KERNEL_IMAGE_BASE: u64 = KERNEL_VMA_BASE;

/// Heap arena: grows downward from `HEAP_TOP`; each growth chunk is followed
/// by an unmapped guard page; bounded below by `HEAP_FLOOR`.
///
/// The window was widened 512 MiB → 896 MiB (top moved from +0x3000_0000 to
/// +0x3800_0000): the old bound turned transient physical fragmentation into
/// a hard arena-exhaustion panic while ~60 MiB of headroom below the kstack
/// window (`KSTACK_VADDR_FLOOR` = +0x3FC0_0000) sat unused.
pub const HEAP_TOP: u64 = KERNEL_VMA_BASE + 0x3800_0000; // top (+896 MiB)
pub const HEAP_FLOOR: u64 = KERNEL_VMA_BASE + 0x1000_0000; //     (+256 MiB)
pub const HEAP_GUARD_PAGES: u64 = 1;
pub const HEAP_GUARD_BYTES: u64 = HEAP_GUARD_PAGES * 4096;

/// Per-task kernel stack size (32 KiB) and the fixed window holding one stack
/// per concurrent kernel task.
///
/// 16 KiB overflowed under debug builds: the unoptimized spawn path
/// (ELF load + caps install + CSPRNG frames) alone consumes >15 KiB of
/// call-chain depth. Debug builds get no inlining, so budget for worst-case
/// frame sizes here rather than per-call-site.
pub const KSTACK_SIZE: u64 = 32 * 1024;
pub const MAX_KSTACKS: usize = 256;
pub const KSTACK_WINDOW_SIZE: u64 = (MAX_KSTACKS as u64) * KSTACK_SIZE;

/// Task-stack window: a fixed range above the heap, inside PML4 slot 511.
///
/// Kernel stacks are mapped here (frame-based, not heap-based) at
/// `KSTACK_VADDR_BASE - slot * KSTACK_SIZE`.  Slot 511's subtree is
/// established before the first `clone_high_half` (the heap maps the same
/// slot during `init`), so every clone shares the window by construction —
/// a stack mapped into the kernel root is visible under any task root, and
/// post-clone mapping is irrelevant because sharing, not snapshotting, is the
/// mechanism (see `clone_high_half`).
pub const KSTACK_VADDR_BASE: u64 = KERNEL_VMA_BASE + 0x4000_0000; // (+1 GiB)
pub const KSTACK_VADDR_FLOOR: u64 = KSTACK_VADDR_BASE - KSTACK_WINDOW_SIZE;

/// Private physmap: DIRECT_MAP maps physical `[0, alloc_end)` here.  Grows
/// upward from this base to cover all of usable RAM; bounded above by the DMA
/// device-window floor (see `DMA_VADDR_FLOOR`), which leaves ~252 GiB of room.
///
/// **Deliberately NOT derived from `KERNEL_VMA_BASE`.**  The physmap is the
/// single largest VA consumer (it must cover *all* RAM) and its base must
/// therefore sit at the bottom of the canonical higher half, far below the
/// kernel image / heap / device windows.  Anchoring it to the kernel base
/// squeezes it next to `u64::MAX` (only 1 GiB of headroom at the current
/// `KERNEL_VMA_BASE`) and overflows the DIRECT_MAP on any machine with more
/// than 1 GiB of RAM.  This value is the Sv39 canonical high-half boundary,
/// is also canonical under x86_64 48-bit paging, and lies inside PML4/L2 slot
/// 511 — so `clone_high_half` (which copies slot 511) carries it into every
/// driver domain automatically.
pub const PHYS_MAP_BASE: u64 = 0xFFFFFFC0_00000000;

// ── Device mapping arenas (below KERNEL_VMA_BASE, edge-to-edge) ───────
//
// These are the live windows used by the ACPI / PCI-ECAM / DMA mappers.
// Each arena allocates *downward* from its BASE toward its FLOOR.

/// ACPI table + GAS MMIO arena.
pub const ACPI_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x1000_0000;
pub const ACPI_VADDR_FLOOR: u64 = KERNEL_VMA_BASE - 0x3000_0000;

/// PCI ECAM config window.
pub const ECAM_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x3000_0000;
pub const ECAM_VADDR_FLOOR: u64 = KERNEL_VMA_BASE - 0x5000_0000;

/// DMA (uncached device buffer) arena.
pub const DMA_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x5000_0000;
pub const DMA_VADDR_FLOOR: u64 = KERNEL_VMA_BASE - 0x7000_0000;

/// Framebuffer device window: the physical scanout framebuffer is mapped here
/// at a fixed higher-half VA so it is visible under user-task page tables
/// (cloned higher half) and writable by `/dev/fb`.  Sits directly below the
/// DMA arena, inside PML4 entry 510 (which `clone_high_half` carries into every
/// task root).
pub const FB_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x7000_0000;
pub const FB_VADDR_FLOOR: u64 = KERNEL_VMA_BASE - 0x9000_0000;

/// Local APIC device window: the LAPIC MMIO page is mapped here at a fixed
/// higher-half VA, directly below the framebuffer arena, still inside PML4
/// entry 510.  The LAPIC cannot stay identity-only: IRQ handlers call
/// `apic_eoi` while the CPU runs on the *process* CR3 (device IRQs fire during
/// syscalls, and syscalls run on the task root), and cloned task roots share
/// only the higher half.
pub const LAPIC_VADDR_BASE: u64 = FB_VADDR_FLOOR - 0x1000_0000;
pub const LAPIC_VADDR_FLOOR: u64 = LAPIC_VADDR_BASE - 0x1000_0000;

/// IOMMU (VT-d) register window: each DRHD's 4 KiB register BAR is mapped
/// here, directly below the LAPIC arena, still inside higher-half PML4
/// sharing (`clone_high_half` copies 256..511). The window is 512 MiB like
/// the other device windows and uses `NO_CACHE` UC mappings.
pub const IOMMU_VADDR_BASE: u64 = LAPIC_VADDR_FLOOR;
pub const IOMMU_VADDR_FLOOR: u64 = IOMMU_VADDR_BASE - 0x2000_0000;

/// Capability supervisor pages: per-process 8K (2×4K) frames mapped supervisor-only
/// (READ, no USER). Must be outside `usermem`'s allocatable range (which caps
/// at `user_ceiling` ≈ stack guard bottom), so the band sits above the user
/// stack top (0x7FFF00000000, worst case after its 256 MiB downward ASLR
/// slide) but below USER_BOUNDARY (0x800000000000). This VA is private per
/// PML4 (low half, not shared via `clone_high_half`'s PML4 256..511 copy)
/// and supervisor-only so ring3 faults.
///
/// `CAP_SLOT_VA` is only the legacy default; every process now picks its own
/// base via [`pick_caps_va`] (per-task randomization of the supervisor
/// window).
pub const CAP_SLOT_VA: u64 = 0x0000_7FFF_8000_0000;
pub const CAP_SLOT_SIZE: u64 = 16384;

/// Randomized caps-window band: starts 2 MiB above the highest possible
/// post-ASLR stack top and spans ~254 MiB (≈15 bits of entropy at 4 KiB
/// granularity), always clear of user regions by construction.
pub const CAP_SLOT_BAND_LO: u64 = 0x0000_7FFF_0200_0000;
pub const CAP_SLOT_BAND_HI: u64 = 0x0000_7FFF_FE00_0000;

/// Pick a fresh page-aligned caps-window base for one process. Collision
/// with user regions is impossible by band construction: user allocations
/// are capped below the stack guard, which tops out under `CAP_SLOT_BAND_LO`.
pub fn pick_caps_va() -> u64 {
    const SPAN: u64 = CAP_SLOT_BAND_HI - CAP_SLOT_BAND_LO;
    let off = crate::random::random_u64() % (SPAN / PAGE_4K);
    CAP_SLOT_BAND_LO + off * PAGE_4K
}

const PAGE_4K: u64 = 4096;

// ── KASLR — 4 MiB CSPRNG, actual slide ───────────────────────────────
//
// KASLR_OFFSET is a 4 MiB-aligned offset within [0x10000000,0x400000000]
// from KERNEL_VMA_BASE, seeded by RDRAND+rdtsc via the early CSPRNG
// (`crate::random::init_early` must run before `init_kaslr`). The kernel
// image is actually remapped at `KERNEL_VMA_BASE - offset` in
// `arch::paging::setup` (true slide, not reserve-only). Candidates that
// would collide with any static region (heap/kstack/physmap/device windows)
// are filtered — with 4 MiB granule and 16 GiB range ≈4096 steps, ~3400
// survive (≈320 with old 2.5 GiB max) vs 3 at 1GiB. Old max 0xA0000000 gave
// only 3 candidates because device windows are contiguous 2.5 GiB – the
// enlarged range uses the large free VA between LAPIC_FLOOR and PHYS_MAP.
pub const KASLR_ALIGN: u64 = 0x400000; // 4 MiB
pub const KASLR_MIN_OFFSET: u64 = 0x1000_0000;
pub const KASLR_MAX_OFFSET: u64 = 0x400000000;
static KASLR_OFFSET: AtomicU64 = AtomicU64::new(0);
static KASLR_INIT_DONE: AtomicBool = AtomicBool::new(false);

pub fn kaslr_offset() -> u64 {
    KASLR_OFFSET.load(Ordering::Relaxed)
}
pub fn set_kaslr_offset(off: u64) {
    KASLR_OFFSET.store(off, Ordering::Relaxed)
}
/// True if `off` would place `[KERNEL_VMA_BASE-off, +KERNEL_IMAGE_SIZE)`
/// over any static region. Used by picker and `verify_layout`.
fn kaslr_collides(off: u64) -> bool {
    if off == 0 {
        return false;
    }
    if off % KASLR_ALIGN != 0 {
        return true;
    }
    if off < KASLR_MIN_OFFSET || off > KASLR_MAX_OFFSET {
        return true;
    }
    let base = KERNEL_VMA_BASE.wrapping_sub(off);
    let end = base.wrapping_add(KERNEL_IMAGE_SIZE);
    // Regions valid pre-paging: physmap_end may be 0 (DIRECT_MAP not yet live), treat as empty.
    let phys_end = physmap_end();
    let check = |r: Range<u64>| !(end <= r.start || r.end <= base);
    if phys_end != 0 && check(PHYS_MAP_BASE..PHYS_MAP_BASE + phys_end) {
        return true;
    }
    if check(HEAP_FLOOR..HEAP_TOP) {
        return true;
    }
    if check(KSTACK_VADDR_FLOOR..KSTACK_VADDR_BASE) {
        return true;
    }
    if check(ACPI_VADDR_FLOOR..ACPI_VADDR_BASE) {
        return true;
    }
    if check(ECAM_VADDR_FLOOR..ECAM_VADDR_BASE) {
        return true;
    }
    if check(DMA_VADDR_FLOOR..DMA_VADDR_BASE) {
        return true;
    }
    if check(FB_VADDR_FLOOR..FB_VADDR_BASE) {
        return true;
    }
    if check(LAPIC_VADDR_FLOOR..LAPIC_VADDR_BASE) {
        return true;
    }
    if check(IOMMU_VADDR_FLOOR..IOMMU_VADDR_BASE) {
        return true;
    }
    false
}

#[cfg(target_arch = "x86_64")]
fn has_rdrand_kaslr() -> bool {
    let cp = core::arch::x86_64::__cpuid(1);
    (cp.ecx & (1 << 30)) != 0
}
#[cfg(target_arch = "x86_64")]
fn rdrand64_kaslr(out: &mut u64) -> bool {
    let mut val: u64 = 0;
    let mut cf: u8 = 0;
    unsafe {
        core::arch::asm!(
            "rdrand {0}",
            "setc {1}",
            out(reg) val,
            out(reg_byte) cf,
            options(nomem, nostack)
        );
    }
    if cf != 0 { *out = val; true } else { false }
}
#[cfg(target_arch = "x86_64")]
fn rdtsc_kaslr() -> u64 {
    let lo: u32; let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (lo as u64) | ((hi as u64) << 32)
}
pub fn init_kaslr() {
    if KASLR_INIT_DONE.swap(true, Ordering::SeqCst) { return; }
    debug_assert!(crate::random::is_ready(), "KASLR: random::init_early must run before init_kaslr");
    // Formal bootargs check: `nokaslr` (or `-nokaslr`) from Multiboot2 tag 1.
    // When present, disable KASLR entirely (offset 0). This is the user-visible
    // switch for debugging / deterministic boots and must be checked before any
    // random pick so the log is unambiguous.
    if crate::bootargs::is_nokaslr() {
        KASLR_OFFSET.store(0, Ordering::Relaxed);
        crate::drivers::serial::SerialPort::puts("[kaslr] disabled via nokaslr (bootargs=\"");
        if let Some(s) = crate::bootargs::get() {
            // Best-effort: truncate to 64 for serial.
            let print_len = core::cmp::min(s.len(), 64);
            crate::drivers::serial::SerialPort::puts(&s[..print_len]);
        }
        crate::drivers::serial::SerialPort::puts("\")\n");
        return;
    }
    let offset = kaslr_pick_offset();
    KASLR_OFFSET.store(offset, Ordering::Relaxed);
    crate::drivers::serial::SerialPort::puts("[kaslr] offset=0x");
    crate::drivers::serial::SerialPort::put_hex(offset);
    crate::drivers::serial::SerialPort::puts(" base=0x");
    let base = KERNEL_VMA_BASE.wrapping_sub(offset);
    crate::drivers::serial::SerialPort::put_hex(base);
    // count candidates for log: all 4MiB slots + zero slide (always valid)
    let mut cnt: u64 = 0;
    let mut off = KASLR_MIN_OFFSET;
    while off <= KASLR_MAX_OFFSET {
        if !kaslr_collides(off) { cnt += 1; }
        off += KASLR_ALIGN;
    }
    if !kaslr_collides(0) {
        cnt += 1;
    }
    crate::drivers::serial::SerialPort::puts(" candidates=");
    crate::drivers::serial::SerialPort::put_u64(cnt);
    crate::drivers::serial::SerialPort::puts("\n");
}
fn kaslr_pick_offset() -> u64 {
    // Enumerate non-colliding 4 MiB slots and pick uniformly via CSPRNG.
    // Early CSPRNG (init_early) is live before this call; if not yet seeded,
    // fall back to standalone RDRAND/jitter (pre-paging, no heap).
    let mut valid_cnt: usize = 0;
    let mut off = KASLR_MIN_OFFSET;
    while off <= KASLR_MAX_OFFSET {
        if !kaslr_collides(off) { valid_cnt += 1; }
        off = off.wrapping_add(KASLR_ALIGN);
    }
    // include 0 no-slide as valid when nothing else? It never collides.
    let has_zero = !kaslr_collides(0);
    let total = valid_cnt + if has_zero { 1 } else { 0 };
    if total == 0 { return 0; }
    let idx = {
        // Prefer CSPRNG (early seeded)
        if crate::random::is_seeded() {
            (crate::random::random_u32() as usize) % total
        } else if crate::random::is_ready() {
            // ready but not yet seeded — still use fill (falls back to splitmix)
            (crate::random::random_u32() as usize) % total
        } else {
            // Last resort: standalone RDRAND/jitter (no heap, pre-RNG)
            let rng_val: u64;
            let have: bool;
            #[cfg(target_arch = "x86_64")]
            {
                let mut tmp_val: u64 = 0;
                let mut tmp_have = false;
                if has_rdrand_kaslr() {
                    for _ in 0..10 { if rdrand64_kaslr(&mut tmp_val) { tmp_have=true; break; } }
                }
                if !tmp_have {
                    let t0 = rdtsc_kaslr();
                    for _ in 0..64 { core::hint::spin_loop(); }
                    let t1 = rdtsc_kaslr();
                    tmp_val = t0 ^ t1.rotate_left(17) ^ crate::drivers::serial::SerialPort::puts as *const () as u64;
                    tmp_have = true;
                }
                rng_val = tmp_val;
                have = tmp_have;
            }
            #[cfg(target_arch = "riscv64")]
            {
                let t0 = crate::arch::riscv64::time::read_time();
                for _ in 0..64 { core::hint::spin_loop(); }
                let t1 = crate::arch::riscv64::time::read_time();
                rng_val = t0 ^ t1.rotate_left(17) ^ 0x6A09E667F3BCC908u64 ^ crate::drivers::serial::SerialPort::puts as *const () as u64;
                have = true;
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
            { rng_val = 0x6A09E667F3BCC908u64; have = true; }
            if !have { return 0; }
            (rng_val as usize) % total
        }
    };
    // map idx -> offset (0 is first entry if present)
    if has_zero {
        if idx == 0 { return 0; }
        let mut cur = 0usize;
        let mut off2 = KASLR_MIN_OFFSET;
        while off2 <= KASLR_MAX_OFFSET {
            if !kaslr_collides(off2) {
                cur += 1;
                if cur == idx { return off2; }
            }
            off2 = off2.wrapping_add(KASLR_ALIGN);
        }
        0
    } else {
        let mut cur = 0usize;
        let mut off2 = KASLR_MIN_OFFSET;
        while off2 <= KASLR_MAX_OFFSET {
            if !kaslr_collides(off2) {
                if cur == idx { return off2; }
                cur += 1;
            }
            off2 = off2.wrapping_add(KASLR_ALIGN);
        }
        0
    }
}

// ── Runtime region table ───────────────────────────────────────────
//
// Each device window allocates *downward* from its `base` (upper bound)
// toward its `floor` (lower bound, exclusive).  `region_next_down()` is the
// single allocator for these windows; the ACPI / PCI-ECAM / DMA mappers no
// longer keep private cursors.

/// One downward-allocating VA window.
pub struct Region {
    name: &'static str,
    base: u64,
    floor: u64,
    next: u64,
}

const fn region(name: &'static str, base: u64, floor: u64) -> Region {
    Region {
        name,
        base,
        floor,
        next: base,
    }
}

/// The live device windows, keyed by name.
static REGIONS: IrqMutex<[Region; 4]> = IrqMutex::new([
    region("acpi", ACPI_VADDR_BASE, ACPI_VADDR_FLOOR),
    region("ecam", ECAM_VADDR_BASE, ECAM_VADDR_FLOOR),
    region("dma", DMA_VADDR_BASE, DMA_VADDR_FLOOR),
    region("iommu", IOMMU_VADDR_BASE, IOMMU_VADDR_FLOOR),
]);

/// Allocate `size` bytes downward inside the named window, page-rounding up.
///
/// Returns the freshly-carped VA, or `None` when the window is exhausted
/// (either overflow or reaching the floor).
pub fn region_next_down(name: &str, size: u64) -> Option<u64> {
    let size = (size + 0xFFF) & !0xFFF;
    let mut regions = REGIONS.lock();
    for r in regions.iter_mut() {
        if r.name == name {
            let vaddr = r.next.checked_sub(size)?;
            if vaddr < r.floor {
                return None;
            }
            r.next = vaddr;
            return Some(vaddr);
        }
    }
    None
}

/// Rewind a window's cursor back to its base. Used by re-init paths.
pub fn region_reset(name: &str) {
    let mut regions = REGIONS.lock();
    for r in regions.iter_mut() {
        if r.name == name {
            r.next = r.base;
            return;
        }
    }
}

static mut PHYS_MAP_END: u64 = 0;
static mut PHYS_MAP_ON: bool = false;

/// Enable the private physmap: records how much RAM is mapped at
/// `[PHYS_MAP_BASE, PHYS_MAP_BASE + end)` and arms the walkers to deref
/// page-table frames through it.
///
/// Must be called only after the DIRECT_MAP region has been mapped into the
/// live page tables *and* those tables have been activated.  Before that the
/// walkers decode physical frames directly (identity).
pub fn init_physmap(end: u64) {
    // The DIRECT_MAP grows to cover all of usable RAM; no fixed ceiling.
    let end = (end + 0x1F_FFFF) & !0x1F_FFFF;
    unsafe {
        PHYS_MAP_END = end;
        PHYS_MAP_ON = true;
    }
}
pub fn physmap_end() -> u64 {
    unsafe { PHYS_MAP_END }
}

/// The physmap offset in effect: `PHYS_MAP_BASE` once the physmap is live and
/// active, otherwise `0` (identity).  Used by the VMM walkers to translate a
/// page-table frame's physical address into the VA they deref.
pub fn phys_offset() -> u64 {
    unsafe { if PHYS_MAP_ON { PHYS_MAP_BASE } else { 0 } }
}

/// Translate a page-table frame's physical address to the VA a walker must
/// deref: the physmap window once enabled, else identity.  `mm`/`arch`/`smp`
/// internals only — no subsystem outside those may use this.
pub fn to_physmap(phys: u64) -> u64 {
    phys.wrapping_add(phys_offset())
}

/// Assert the static regions do not overlap. Called once early in `init`.
/// Also validates the KASLR slide (`KERNEL_VMA_BASE - kaslr`) against *all*
/// static regions so the actual remapped image never collides.
pub fn verify_layout() {
    let kaslr = kaslr_offset();
    let regions: [(&str, Range<u64>); 9] = [
        ("heap", HEAP_FLOOR..HEAP_TOP),
        ("kstack", KSTACK_VADDR_FLOOR..KSTACK_VADDR_BASE),
        ("physmap", PHYS_MAP_BASE..PHYS_MAP_BASE + physmap_end()),
        ("acpi", ACPI_VADDR_FLOOR..ACPI_VADDR_BASE),
        ("ecam", ECAM_VADDR_FLOOR..ECAM_VADDR_BASE),
        ("dma", DMA_VADDR_FLOOR..DMA_VADDR_BASE),
        ("fb", FB_VADDR_FLOOR..FB_VADDR_BASE),
        ("lapic", LAPIC_VADDR_FLOOR..LAPIC_VADDR_BASE),
        ("iommu", IOMMU_VADDR_FLOOR..IOMMU_VADDR_BASE),
    ];
    if kaslr != 0 {
        assert!(!kaslr_collides(kaslr), "KASLR offset {:#x} collides (filtered pick failed)", kaslr);
        let kaslr_base = KERNEL_VMA_BASE.wrapping_sub(kaslr);
        let kaslr_range = kaslr_base..kaslr_base + KERNEL_IMAGE_SIZE;
        for (name, r) in &regions {
            assert!(
                kaslr_range.end <= r.start || r.end <= kaslr_range.start,
                "KASLR [{:#x},{:#x}) overlaps {} [{:#x},{:#x})",
                kaslr_range.start,
                kaslr_range.end,
                name,
                r.start,
                r.end
            );
        }
    }
    for (i, (an, ar)) in regions.iter().enumerate() {
        for (bn, br) in &regions[i + 1..] {
            assert!(
                ar.end <= br.start || br.end <= ar.start,
                "virtual layout overlap {} [{:#x},{:#x}) vs {} [{:#x},{:#x})",
                an,
                ar.start,
                ar.end,
                bn,
                br.start,
                br.end
            );
        }
    }
}
