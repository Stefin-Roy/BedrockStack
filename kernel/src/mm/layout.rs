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
use spin::Mutex;

///
/// Start of the canonical higher-half region (also Start of x86_64's standard
/// negative-address kernel range and Sv39's sign-extended high half).
pub const KERNEL_VMA_BASE: u64 = 0xFFFFFF80_00000000;

/// Size reserved for the higher-half kernel image (Phase E target).
pub const KERNEL_IMAGE_SIZE: u64 = 0x1000_0000; // 256 MiB
pub const KERNEL_IMAGE_BASE: u64 = KERNEL_VMA_BASE;

/// Heap arena: grows downward from `HEAP_TOP`; each growth chunk is followed
/// by an unmapped guard page; bounded below by `HEAP_FLOOR`.
pub const HEAP_TOP: u64   = KERNEL_VMA_BASE + 0x3000_0000; // top  (+768 MiB)
pub const HEAP_FLOOR: u64 = KERNEL_VMA_BASE + 0x1000_0000; //      (+256 MiB)
pub const HEAP_GUARD_PAGES: u64 = 1;
pub const HEAP_GUARD_BYTES: u64 = HEAP_GUARD_PAGES * 4096;

/// Private physmap: DIRECT_MAP maps physical `[0, alloc_end)` here.  Grows
/// upward from this base to cover all of usable RAM (no fixed ceiling — the
/// window extends into the canonical half above, which has no neighbours).
pub const PHYS_MAP_BASE: u64 = KERNEL_VMA_BASE + 0x4000_0000; // +1 GiB

// ── Device mapping arenas (below KERNEL_VMA_BASE, edge-to-edge) ───────
//
// These are the live windows used by the ACPI / PCI-ECAM / DMA mappers.
// Each arena allocates *downward* from its BASE toward its FLOOR.

/// ACPI table + GAS MMIO arena.
pub const ACPI_VADDR_BASE: u64  = KERNEL_VMA_BASE - 0x1000_0000;
pub const ACPI_VADDR_FLOOR: u64 = KERNEL_VMA_BASE - 0x3000_0000;

/// PCI ECAM config window.
pub const ECAM_VADDR_BASE: u64  = KERNEL_VMA_BASE - 0x3000_0000;
pub const ECAM_VADDR_FLOOR: u64 = KERNEL_VMA_BASE - 0x5000_0000;

/// DMA (uncached device buffer) arena.
pub const DMA_VADDR_BASE: u64   = KERNEL_VMA_BASE - 0x5000_0000;
pub const DMA_VADDR_FLOOR: u64  = KERNEL_VMA_BASE - 0x7000_0000;

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
    Region { name, base, floor, next: base }
}

/// The live device windows, keyed by name.
static REGIONS: Mutex<[Region; 3]> = Mutex::new([
    region("acpi", ACPI_VADDR_BASE, ACPI_VADDR_FLOOR),
    region("ecam", ECAM_VADDR_BASE, ECAM_VADDR_FLOOR),
    region("dma",  DMA_VADDR_BASE,  DMA_VADDR_FLOOR),
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
    unsafe {
        if PHYS_MAP_ON {
            PHYS_MAP_BASE
        } else {
            0
        }
    }
}

/// Translate a page-table frame's physical address to the VA a walker must
/// deref: the physmap window once enabled, else identity.  `mm`/`arch`/`smp`
/// internals only — no subsystem outside those may use this.
pub fn to_physmap(phys: u64) -> u64 {
    phys.wrapping_add(phys_offset())
}

/// Assert the static regions do not overlap. Called once early in `init`.
pub fn verify_layout() {
    let regions: [(&str, Range<u64>); 5] = [
        ("heap",   HEAP_FLOOR..HEAP_TOP),
        ("physmap", PHYS_MAP_BASE..PHYS_MAP_BASE + physmap_end()),
        ("acpi",   ACPI_VADDR_FLOOR..ACPI_VADDR_BASE),
        ("ecam",   ECAM_VADDR_FLOOR..ECAM_VADDR_BASE),
        ("dma",    DMA_VADDR_FLOOR..DMA_VADDR_BASE),
    ];
    for (i, (an, ar)) in regions.iter().enumerate() {
        for (bn, br) in &regions[i + 1..] {
            assert!(
                ar.end <= br.start || br.end <= ar.start,
                "virtual layout overlap {} [{:#x},{:#x}) vs {} [{:#x},{:#x})",
                an, ar.start, ar.end, bn, br.start, br.end
            );
        }
    }
}