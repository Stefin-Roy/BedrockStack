//! Virtual Memory Manager — architecture-agnostic page table abstraction.
//!
//! Provides `Vmm`, an object that holds a page table root and supports
//! on-demand `map` / `unmap` / `translate` operations after the table is
//! live.  Arch-specific page-table walks live in the sibling modules
//! `x86_64` and `riscv64`.

use crate::mm::phys_alloc::BitmapAllocator;

// Re-export arch-specific activation helpers so callers can switch tables.
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::activate;
#[cfg(target_arch = "riscv64")]
pub use self::riscv64::activate;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "riscv64")]
mod riscv64;

// ── Page flags (architecture-independent) ───────────────────────────

/// Page permissions and attributes.
///
/// These are translated to the native PTE flags inside each arch module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFlags(u8);

#[allow(dead_code)]
impl PageFlags {
    pub const EMPTY:   Self = Self(0);
    pub const READ:    Self = Self(1 << 0);
    pub const WRITE:   Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const NO_CACHE: Self = Self(1 << 3);
    pub const USER:    Self = Self(1 << 4); // future user-space

    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn bits(self) -> u8 {
        self.0
    }
}

// Allow combining flags with `|`.
impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitOrAssign for PageFlags {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}
impl core::ops::BitAnd for PageFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for PageFlags {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}
impl core::ops::BitAndAssign for PageFlags {
    fn bitand_assign(&mut self, rhs: Self) { self.0 &= rhs.0; }
}

// ── Constants ─────────────────────────────────────────────────────────

/// Base address for the higher-half kernel alias mapping.
///
/// The kernel image is linked at its physical address (e.g. `0x400000` on
/// x86_64).  Phase 2 of the VMM adds an *alias* mapping so that every kernel
/// page is also reachable at `KERNEL_VMA_BASE + phys_addr`.  This gives us a
/// higher-half view without changing the linker script or the code's
/// compiled addresses.
pub const KERNEL_VMA_BASE: u64 = 0xFFFFFF8000000000;

// ── Vmm ─────────────────────────────────────────────────────────────

/// A page table root object that can be queried and modified at run time.
pub struct Vmm {
    root: u64, // physical address of the root table (PML4 / L2)
}

impl Vmm {
    /// Allocate a fresh, empty page table (one zeroed root frame).
    pub fn new(alloc: &mut BitmapAllocator) -> Self {
        let root = alloc.alloc().expect("VMM: OOM for root page table");
        // Zero the frame.
        unsafe {
            core::ptr::write_bytes(root as *mut u8, 0, 4096);
        }
        Vmm { root }
    }

    pub fn root(&self) -> u64 { self.root }

    // ── Mapping ─────────────────────────────────────────────────────

    /// Map a single 4 KiB page.
    ///
    /// # Panics
    /// - If `vaddr` or `paddr` are not 4 KiB aligned.
    /// - If the page is already mapped (prevents double-map bugs).
    /// - If the allocator runs out of frames for intermediate tables.
    pub fn map_4k(
        &mut self,
        alloc: &mut BitmapAllocator,
        vaddr: u64,
        paddr: u64,
        flags: PageFlags,
    ) {
        assert_eq!(vaddr & 0xFFF, 0, "VMM: vaddr not 4K aligned");
        assert_eq!(paddr & 0xFFF, 0, "VMM: paddr not 4K aligned");
        #[cfg(target_arch = "x86_64")]
        x86_64::map_4k(self.root, alloc, vaddr, paddr, flags);
        #[cfg(target_arch = "riscv64")]
        riscv64::map_4k(self.root, alloc, vaddr, paddr, flags);
    }

    /// Map a 2 MiB huge page.
    ///
    /// # Panics
    /// - If `vaddr` or `paddr` are not 2 MiB aligned.
    /// - If any page in the range is already mapped.
    /// - If the allocator runs out of frames for intermediate tables.
    pub fn map_2m(
        &mut self,
        alloc: &mut BitmapAllocator,
        vaddr: u64,
        paddr: u64,
        flags: PageFlags,
    ) {
        assert_eq!(vaddr & 0x1F_FFFF, 0, "VMM: vaddr not 2M aligned");
        assert_eq!(paddr & 0x1F_FFFF, 0, "VMM: paddr not 2M aligned");
        #[cfg(target_arch = "x86_64")]
        x86_64::map_2m(self.root, alloc, vaddr, paddr, flags);
        #[cfg(target_arch = "riscv64")]
        riscv64::map_2m(self.root, alloc, vaddr, paddr, flags);
    }

    /// Convenience: map a range, auto-selecting 2 MiB vs 4 KiB pages.
    ///
    /// The address range `[vaddr, vaddr + size)` is mapped to the
    /// *contiguous* physical range starting at `paddr`.
    ///
    /// # Panics
    /// - If `vaddr` or `paddr` are not page-aligned.
    /// - On any mapping failure.
    pub fn map(
        &mut self,
        alloc: &mut BitmapAllocator,
        vaddr: u64,
        paddr: u64,
        size: u64,
        flags: PageFlags,
    ) {
        assert_eq!(vaddr & 0xFFF, 0, "VMM: vaddr not page-aligned");
        assert_eq!(paddr & 0xFFF, 0, "VMM: paddr not page-aligned");
        assert!(size > 0);

        let mut remaining = size;
        let mut v = vaddr;
        let mut p = paddr;

        // Try 2 MiB chunks when both ends are aligned.
        while remaining >= 2 * 1024 * 1024 && (v & 0x1F_FFFF) == 0 && (p & 0x1F_FFFF) == 0 {
            self.map_2m(alloc, v, p, flags);
            v += 2 * 1024 * 1024;
            p += 2 * 1024 * 1024;
            remaining -= 2 * 1024 * 1024;
        }

        // Remainder with 4 KiB pages.
        while remaining > 0 {
            self.map_4k(alloc, v, p, flags);
            v += 4096;
            p += 4096;
            remaining -= 4096;
        }
    }

    // ── Unmapping ───────────────────────────────────────────────────

    /// Unmap the 4 KiB page at `vaddr`.
    ///
    /// Returns `false` if the page was not mapped.
    pub fn unmap_4k(&mut self, alloc: &mut BitmapAllocator, vaddr: u64) -> bool {
        assert_eq!(vaddr & 0xFFF, 0, "VMM: vaddr not 4K aligned");
        #[cfg(target_arch = "x86_64")]
        return x86_64::unmap_4k(self.root, alloc, vaddr);
        #[cfg(target_arch = "riscv64")]
        return riscv64::unmap_4k(self.root, alloc, vaddr);
    }

    /// Unmap a range of pages (4 KiB granularity).
    pub fn unmap(&mut self, alloc: &mut BitmapAllocator, vaddr: u64, size: u64) {
        assert_eq!(vaddr & 0xFFF, 0);
        let mut v = vaddr;
        let end = vaddr + size;
        while v < end {
            self.unmap_4k(alloc, v);
            v += 4096;
        }
    }

    // ── Translation ─────────────────────────────────────────────────

    /// Translate a virtual address to the physical address it maps to.
    /// Returns `None` if the address is not mapped.
    pub fn translate(&self, vaddr: u64) -> Option<u64> {
        #[cfg(target_arch = "x86_64")]
        return x86_64::translate(self.root, vaddr);
        #[cfg(target_arch = "riscv64")]
        return riscv64::translate(self.root, vaddr);
    }
}
