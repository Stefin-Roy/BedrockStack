//! Virtual Memory Manager — architecture-agnostic page table abstraction.
//!
//! Provides `Vmm`, an object that holds a page table root and supports
//! on-demand `map` / `unmap` / `translate` operations after the table is
//! live.  Arch-specific page-table walks live in the sibling modules
//! `x86_64` and `riscv64`.

use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_arch = "x86_64")]
use spin::Mutex;

use crate::mm::phys_alloc::BitmapAllocator;

// Re-export arch-specific activation helpers so callers can switch tables.
#[cfg(target_arch = "riscv64")]
pub use self::riscv64::activate;
#[cfg(target_arch = "riscv64")]
pub use self::riscv64::translate_user;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::activate;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::clone_high_half;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::destroy_root;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::{
    clone_user_space_cow,
    edit_user_leaf, edit_user_leaf_range, user_leaf_make_writable, user_leaf_make_writable_range,
    user_leaf_repoint_writable, user_leaf_set_pkey_range, user_leaf_write_protect,
    user_leaf_write_protect_range,
};
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::init_pat_wc;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::make_read_only;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::prepopulate_window;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::{translate_user, translate_user_range_ok};
#[cfg(target_arch = "riscv64")]
pub use self::riscv64::translate_user_range_ok;

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

// ── Page flags (architecture-independent) ───────────────────────────

/// Page permissions and attributes.
///
/// These are translated to the native PTE flags inside each arch module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFlags(u8);

impl PageFlags {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const NO_CACHE: Self = Self(1 << 3);
    pub const USER: Self = Self(1 << 4); // future user-space
    pub const WRITE_COMBINING: Self = Self(1 << 5); // WC (via PAT on x86_64)

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
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl core::ops::BitOrAssign for PageFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl core::ops::BitAnd for PageFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
impl core::ops::Not for PageFlags {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}
impl core::ops::BitAndAssign for PageFlags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

// ── Constants ─────────────────────────────────────────────────────────

/// Base address for the higher-half kernel alias mapping.
///
/// The kernel image is linked at its physical address (e.g. `0x400000` on
/// x86_64).  Phase 2 of the VMM adds an *alias* mapping so that every kernel
/// page is also reachable at `KERNEL_VMA_BASE + phys_addr`.  This gives us a
/// higher-half view without changing the linker script or the code's
/// compiled addresses.
pub const KERNEL_VMA_BASE: u64 = crate::mm::layout::KERNEL_VMA_BASE;

// ── Vmm ─────────────────────────────────────────────────────────────

/// A page table root object that can be queried and modified at run time.
pub struct Vmm {
    root: u64, // physical address of the root table (PML4 / L2)
    alloc: *mut BitmapAllocator,
}

unsafe impl Send for Vmm {}
unsafe impl Sync for Vmm {}

impl Vmm {
    /// Allocate a fresh, empty page table (one zeroed root frame).
    pub fn new(alloc: &mut BitmapAllocator) -> Self {
        let root = alloc.alloc().expect("VMM: OOM for root page table");
        // Zero the frame (through the physmap — the identity window no longer
        // covers all of RAM, so a raw physical write would fault).
        unsafe {
            core::ptr::write_bytes(crate::mm::layout::to_physmap(root) as *mut u8, 0, 4096);
        }
        Vmm {
            root,
            alloc: alloc as *mut BitmapAllocator,
        }
    }

    /// Wrap an existing root frame — no allocation.  The internal allocator
    /// pointer is taken from the global physical allocator if it is already
    /// registered, otherwise `null` (callers that map must pass an allocator
    /// to the inherent methods, or call `set_alloc` first).
    pub fn from_root(root: u64) -> Self {
        Vmm {
            root,
            alloc: crate::mm::heap::phys_allocator_raw(),
        }
    }

    /// Bind an allocator to this VMM so the allocator-independent trait
    /// methods (`VirtualMemoryManager::map`) can allocate intermediate tables.
    pub fn set_alloc(&mut self, alloc: *mut BitmapAllocator) {
        self.alloc = alloc;
    }

    /// The allocator bound to this VMM (may be null until `set_alloc`).
    pub fn allocator(&self) -> *mut BitmapAllocator {
        self.alloc
    }

    pub fn root(&self) -> u64 {
        self.root
    }

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
        assert!(size > 0, "VMM: size must be > 0");
        assert_eq!(size & 0xFFF, 0, "VMM: size must be page-aligned");

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

    /// Clear the leaf PTE at `vaddr` and collect any orphaned intermediate
    /// page-table frames into `pending` (no TLB flush, no frame freeing).
    ///
    /// Shared by the single-page and range unmap paths so the cross-CPU TLB
    /// shootdown runs exactly once per range.
    fn unmap_page_collect(&mut self, pending: &mut PendingFrames, vaddr: u64) -> bool {
        #[cfg(target_arch = "x86_64")]
        return x86_64::unmap_4k(self.root, vaddr, pending);
        #[cfg(target_arch = "riscv64")]
        return riscv64::unmap_4k(self.root, vaddr, pending);
    }

    /// Clear the leaf PTE at `vaddr` *without* reclaiming intermediate tables.
    ///
    /// Range unmap uses this so it can reclaim each touched page table once
    /// (via [`Self::reclaim_tables`]) instead of once per 4 KiB page.
    fn unmap_page_collect_noreclaim(&mut self, pending: &mut PendingFrames, vaddr: u64) -> bool {
        #[cfg(target_arch = "x86_64")]
        return x86_64::unmap_4k_no_reclaim(self.root, vaddr, pending);
        #[cfg(target_arch = "riscv64")]
        return riscv64::unmap_4k(self.root, vaddr, pending);
    }

    /// Reclaim intermediate page tables for a 2 MiB PT-group whose leaves are
    /// all already cleared.  Must be followed by a TLB flush + shootdown
    /// before the collected frames are released to the allocator.
    #[cfg(target_arch = "x86_64")]
    fn reclaim_tables(&self, pending: &mut PendingFrames, first_cleared: u64) {
        // Clone roots share higher-half subtrees; reclaim is deferred while any clone lives.
        // The x86_64 helper keeps `keep_frames` guard — no assert here so normal process teardown
        // (which has live clones) does not panic in debug (`panic=abort` is fatal).
        if has_clone_roots() {
            return;
        }
        x86_64::reclaim_empty_tables(self.root, pending, first_cleared);
    }

    /// riscv64 reclaims empty intermediate tables inline inside
    /// `riscv64::unmap_4k` (per page), so this range-path hook stays a no-op
    /// there — the collector is drained by the per-page path instead.
    #[cfg(target_arch = "riscv64")]
    fn reclaim_tables(&self, _pending: &mut PendingFrames, _first_cleared: u64) {}

    /// Unmap the 4 KiB page at `vaddr`.
    ///
    /// Returns `false` if the page was not mapped.
    ///
    /// After clearing the PTE this flushes the local TLB, broadcasts a full
    /// TLB shootdown to every online CPU, and only then returns any orphaned
    /// page-table frames to `alloc` — so no CPU can hold a stale mapping to a
    /// frame that is being released.
    pub fn unmap_4k(&mut self, alloc: &mut BitmapAllocator, vaddr: u64) -> bool {
        assert_eq!(vaddr & 0xFFF, 0, "VMM: vaddr not 4K aligned");
        let mut pending = PendingFrames::new();
        let removed = self.unmap_page_collect(&mut pending, vaddr);
        if removed {
            flush_tlb();
            shootdown_scoped(range_is_low_half(vaddr, 4096), self.root);
            crate::services::dma::invalidate_trans_cache(vaddr, 4096);
            pending.flush(alloc);
        }
        removed
    }

    /// Unmap a range of pages (4 KiB granularity).
    ///
    /// All leaf PTEs are cleared first, then a single full TLB shootdown is
    /// broadcast before any freed frame (intermediate tables or, by the
    /// caller, the leaves) is released to the allocator.
    pub fn unmap(&mut self, alloc: &mut BitmapAllocator, vaddr: u64, size: u64) {
        assert_eq!(vaddr & 0xFFF, 0);
        // The range is contiguous, so its half is decided once up front; a
        // low-half-only range skips IPIing CPUs running other roots.
        let low_only = range_is_low_half(vaddr, size);
        let root = self.root;
        let mut pending = PendingFrames::new();
        let mut v = vaddr;
        let end = vaddr + size;
        let mut removed_any = false;
        // `group_first` is the first cleared page of the current 2 MiB PT-group.
        // Reclaim runs once per touched table (all its leaves clear in address
        // order) rather than once per 4 KiB page.
        let mut group_first: u64 = u64::MAX;
        while v < end {
            if self.unmap_page_collect_noreclaim(&mut pending, v) {
                removed_any = true;
                let group = v >> 21;
                if group != group_first >> 21 {
                    if group_first != u64::MAX {
                        if pending.remaining() < 3 {
                            if removed_any || !pending.is_empty() {
                                flush_tlb();
                                shootdown_scoped(low_only, root);
                            }
                            pending.flush(alloc);
                            removed_any = false;
                        }
                        self.reclaim_tables(&mut pending, group_first);
                        if pending.is_full() {
                            flush_tlb();
                            shootdown_scoped(low_only, root);
                            pending.flush(alloc);
                            removed_any = false;
                        }
                    }
                    group_first = v;
                }
            }
            v += 4096;
            if pending.is_full() {
                // Always flush+shootdown when we have pending table frames,
                // even if removed_any is false (orphaned intermediate tables).
                if removed_any || !pending.is_empty() {
                    flush_tlb();
                    shootdown_scoped(low_only, root);
                }
                pending.flush(alloc);
                removed_any = false;
            }
        }
        if group_first != u64::MAX {
            if pending.remaining() < 3 && !pending.is_empty() {
                flush_tlb();
                shootdown_scoped(low_only, root);
                pending.flush(alloc);
            }
            self.reclaim_tables(&mut pending, group_first);
        }
        if removed_any || !pending.is_empty() {
            flush_tlb();
            shootdown_scoped(low_only, root);
            crate::services::dma::invalidate_trans_cache(vaddr, size);
            pending.flush(alloc);
        } else {
            // Even if no leaf was removed, the range may have been cached.
            crate::services::dma::invalidate_trans_cache(vaddr, size);
        }
    }

    /// Unmap a range of pages (4 KiB granularity), collecting the backing
    /// physical frames into `frames` so the caller can free them.
    ///
    /// This is the batched sibling of the per-page `translate`+`unmap_4k`
    /// dance: every PTE in the range is cleared first, then a *single* TLB
    /// shootdown (or one per `PENDING_CAPACITY` orphaned table frames) is
    /// broadcast before any leaf frame lands in `frames` or any orphaned
    /// intermediate table frame is released.  Releasing leaves one-at-a-time
    /// through `unmap_4k` broadcasts a full cross-CPU shootdown per page, which
    /// turns a 100-page `brk` shrink into 100 IPI storms; this collapses the
    /// whole range into one (plus one per `PENDING_CAPACITY` orphaned table
    /// frames).  Intermediate tables are reclaimed once per touched 2 MiB
    /// group rather than once per 4 KiB page, skipping the redundant
    /// 512-entry emptiness scans.
    ///
    /// `frames` is appended to, never cleared.  Its contents are valid to free
    /// once this returns (all affected PTEs are flushed system-wide by then).
    pub fn unmap_range_collect(
        &mut self,
        alloc: &mut BitmapAllocator,
        vaddr: u64,
        size: u64,
        frames: &mut Vec<u64>,
    ) {
        assert_eq!(vaddr & 0xFFF, 0);
        assert_eq!(size & 0xFFF, 0);
        let low_only = range_is_low_half(vaddr, size);
        let root = self.root;
        let mut pending = PendingFrames::new();
        let mut v = vaddr;
        let end = vaddr + size;
        let mut removed_any = false;
        // `group_first` is the first cleared page of the current 2 MiB PT-group.
        // Reclaim runs once per touched table (all its leaves clear in address
        // order) rather than once per 4 KiB page.
        let mut group_first: u64 = u64::MAX;
        while v < end {
            if let Some(phys) = self.translate(v) {
                if self.unmap_page_collect_noreclaim(&mut pending, v) {
                    frames.push(phys);
                    removed_any = true;
                    let group = v >> 21;
                    if group != group_first >> 21 {
                        if group_first != u64::MAX {
                            if pending.remaining() < 3 {
                                if removed_any || !pending.is_empty() {
                                    flush_tlb();
                                    shootdown_scoped(low_only, root);
                                }
                                pending.flush(alloc);
                                removed_any = false;
                            }
                            self.reclaim_tables(&mut pending, group_first);
                            if pending.is_full() {
                                flush_tlb();
                                shootdown_scoped(low_only, root);
                                pending.flush(alloc);
                                removed_any = false;
                            }
                        }
                        group_first = v;
                    }
                }
            }
            v += 4096;
            if pending.is_full() {
                if removed_any || !pending.is_empty() {
                    flush_tlb();
                    shootdown_scoped(low_only, root);
                }
                pending.flush(alloc);
                removed_any = false;
            }
        }
        if group_first != u64::MAX {
            if pending.remaining() < 3 && !pending.is_empty() {
                flush_tlb();
                shootdown_scoped(low_only, root);
                pending.flush(alloc);
            }
            self.reclaim_tables(&mut pending, group_first);
        }
        if removed_any || !pending.is_empty() {
            flush_tlb();
            shootdown_scoped(low_only, root);
            crate::services::dma::invalidate_trans_cache(vaddr, size);
            pending.flush(alloc);
        } else {
            crate::services::dma::invalidate_trans_cache(vaddr, size);
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

    /// Flush the TLB (entirety).  Delegates to the arch-agnostic free fn.
    pub fn flush_tlb(&self) {
        flush_tlb()
    }
}

/// Flush the TLB for the whole address space, local to the calling CPU only.
///
/// The `Vmm` bound is not required by the operation itself, so this is exposed
/// as a free function for code (e.g. `unmap_4k`) that reclaims page-table
/// frames and needs a full flush without holding a `Vmm`.
///
/// This only invalidates the *local* CPU's TLB.  When frames are about to be
/// returned to the physical allocator, callers must also run
/// [`shootdown_tlb`] so every online CPU invalidates before the frames can be
/// reallocated.
pub fn flush_tlb() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        // Prefer INVPCID(type2 = all-context) when CR4.PCIDE is set and CPU
        // advertises INVPCID (CPUID leaf 7 EBX[10]). Falls back to CR3 reload.
        // Range paths use this same full flush plus shootdown_tlb; per-page
        // INVLPG batching lives in unmap_4k_impl's local flush.
        if has_pcide() && has_invpcid() {
            invpcid_all();
        } else {
            core::arch::asm!(
                "mov rax, cr3; mov cr3, rax",
                options(nostack, preserves_flags)
            );
        }
    }
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("sfence.vma", options(nostack));
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn has_pcide() -> bool {
    let cr4: u64;
    unsafe { core::arch::asm!("mov {0}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    (cr4 & (1 << 17)) != 0
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn has_invpcid() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 {
        return false;
    }
    let res = core::arch::x86_64::__cpuid_count(7, 0);
    (res.ebx & (1 << 10)) != 0
}

#[cfg(target_arch = "x86_64")]
#[repr(C, align(16))]
struct InvpcidDesc {
    pcid: u64,
    addr: u64,
}

#[cfg(target_arch = "x86_64")]
unsafe fn invpcid_all() {
    // Type 2 = all-context invalidation (includes globals)
    let desc = InvpcidDesc { pcid: 0, addr: 0 };
    unsafe {
        core::arch::asm!(
            "invpcid {0}, [{1}]",
            in(reg) 2u64,
            in(reg) &desc,
            options(nostack, preserves_flags)
        );
    }
}

// ── Cross-CPU TLB shootdown ───────────────────────────────────────────
//
// Every CPU in the system shares the active higher-half page tables (the boot
// root, and any roots produced by `clone_high_half`).  Unmapping a page and
// freeing its frames on one CPU is unsafe while another CPU may hold a stale
// TLB entry for that VA — the freed frame can be reallocated immediately and
// the stale entry would read/write the new owner's memory.
//
// The x86_64 mappings never set the `GLOBAL` (PGE) flag, so a full CR3 reload
// (`flush_tlb`) invalidates the entire TLB on the executing CPU; the same
// `sfence.vma` is used on riscv64.  A shootdown therefore only needs every
// CPU to run that flush before any freed frame is released to the allocator.

/// Serialize all page-table mutations (map / unmap / table reclamation).
///
/// PreemptMutex (preemption disabled, IRQs stay enabled while spinning) so a
/// CPU blocked here can still take an IPI and acknowledge a TLB shootdown —
/// holding this lock across the shootdown *wait* is never done (see
/// [`shootdown_tlb`]). Full preemption: holder cannot be preempted and
/// deadlock spinner on BSP.
pub(crate) fn lock() -> crate::sync::PreemptGuard<'static, ()> {
    static VMM_LOCK: crate::sync::PreemptMutex<()> = crate::sync::PreemptMutex::new(());
    VMM_LOCK.lock()
}

/// Monotonic shootdown generation.  A target CPU acknowledges the latest
/// generation it has flushed, so overlapping shootdowns cannot be confused.
static TLB_SEQ: AtomicU64 = AtomicU64::new(0);

/// Boundary between the low (per-address-space) and high (shared kernel)
/// halves of canonical virtual address space.  Bit 47 set ⇒ higher half on
/// x86_64; the same value works as the Sv39 sign-bit boundary on riscv64.
const HALF_BOUNDARY: u64 = 0x0000_8000_0000_0000;

/// Root each CPU currently runs on (0 = unknown / early boot).
///
/// Written before every CR3/satp switch (`set_current_root`); read by
/// shootdown targeting so *low-half* (user space) flushes only IPI CPUs
/// actually running that root.  Sound because CR3 loads are untagged (no
/// PCID) and no page uses the GLOBAL flag — every root switch fully flushes
/// non-global entries, so a CPU not on `root` cannot hold stale entries for
/// it.  Higher-half mutations always broadcast (the high half is shared by
/// every clone), so targeting only ever narrows user-space flushes.
static CPU_ROOT: [AtomicU64; crate::smp::MAX_CPUS] =
    [const { AtomicU64::new(0) }; crate::smp::MAX_CPUS];

/// Record the page-table root this CPU is about to run on.  Must be called
/// immediately before every root switch (context switch, `activate`, idle
/// transition).
pub fn set_current_root(root: u64) {
    let cpu = crate::smp::current_cpu_id() as usize;
    if cpu < crate::smp::MAX_CPUS {
        CPU_ROOT[cpu].store(root, Ordering::Relaxed);
    }
}

pub fn current_root() -> u64 {
    let cpu = crate::smp::current_cpu_id() as usize;
    if cpu < crate::smp::MAX_CPUS {
        CPU_ROOT[cpu].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Snapshot for unispace: (clone_roots, tlb_seq, half_boundary)
pub fn vmm_global_snapshot() -> (usize, u64, u64) {
    #[cfg(target_arch = "x86_64")]
    let clones = CLONE_ROOTS.load(Ordering::SeqCst);
    #[cfg(not(target_arch = "x86_64"))]
    let clones = 0usize;
    let seq = TLB_SEQ.load(Ordering::Acquire);
    (clones, seq, HALF_BOUNDARY)
}

pub fn vmm_cpu_roots_snapshot() -> [u64; crate::smp::MAX_CPUS] {
    let mut out = [0u64; crate::smp::MAX_CPUS];
    for i in 0..crate::smp::MAX_CPUS {
        out[i] = CPU_ROOT[i].load(Ordering::Relaxed);
    }
    out
}

pub fn vmm_tlb_acks_snapshot() -> [u64; crate::smp::MAX_CPUS] {
    let mut out = [0u64; crate::smp::MAX_CPUS];
    for i in 0..crate::smp::MAX_CPUS {
        out[i] = TLB_ACK[i].load(Ordering::Acquire);
    }
    out
}

pub fn vmm_clone_roots_snapshot() -> alloc::vec::Vec<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        clone_roots()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        alloc::vec::Vec::new()
    }
}

/// Per-CPU acknowledgement: the highest shootdown generation each CPU has
/// flushed and acknowledged.
static TLB_ACK: [AtomicU64; crate::smp::MAX_CPUS] =
    [const { AtomicU64::new(0) }; crate::smp::MAX_CPUS];

/// Publish a shootdown acknowledgement without ever moving it backwards.
///
/// Shootdowns may overlap on different CPUs.  A plain `store(seq)` is unsafe:
/// a CPU can acknowledge a newer generation from an IPI and then resume an
/// older local shootdown which stores its smaller sequence number.  The CPU
/// that issued the newer shootdown would then wait forever for an
/// acknowledgement that had already happened.
fn acknowledge_tlb(seq: u64) {
    let cpu = crate::smp::current_cpu_id() as usize;
    let ack = &TLB_ACK[cpu];
    let mut current = ack.load(Ordering::Acquire);
    while current < seq {
        match ack.compare_exchange_weak(current, seq, Ordering::Release, Ordering::Acquire) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Number of live cloned roots (from `clone_high_half`).  Clones share the
/// parent's higher-half PDPT/PD/PT subtrees, so intermediate table frames can
/// never be freed while a clone exists.  x86_64-only: riscv64 shares a single
/// root and never reclaims intermediate tables.
#[cfg(target_arch = "x86_64")]
static CLONE_ROOTS: AtomicUsize = AtomicUsize::new(0);

/// Live cloned roots, kept in step with `CLONE_ROOTS`: pushes on
/// `register_clone_root`, removal on `unregister_clone_root`.  Iterated by
/// `sync_clone_half` so a higher-half PML4 slot populated after a clone was
/// born is copied into that clone too (sharing, not snapshotting, is the
/// invariant — but a slot that was *absent* at clone time has nothing to share
/// until it is synced).
#[cfg(target_arch = "x86_64")]
static CLONES: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Record a cloned root so table-frame reclamation is disabled for the parent
/// and so `sync_clone_half` can keep it sharing the higher half.
///
/// Called by `clone_high_half` once the new root exists.  Clones are torn down
/// via `destroy_root`, which calls `unregister_clone_root` once its frames are
/// released, so the count/registry reflect the *live* clones.
#[cfg(target_arch = "x86_64")]
pub fn register_clone_root(root: u64) {
    CLONE_ROOTS.fetch_add(1, Ordering::SeqCst);
    CLONES.lock().push(root);
}

/// Release a cloned root that has been destroyed, re-arming empty-table
/// reclamation on the parent once no clone remains.
///
/// Returns `true` when this was the LAST live clone — the caller (x86_64
/// `destroy_root`) then sweeps the parent's higher half to recover tables
/// that were deliberately leaked while clones shared them.
///
/// Called by `destroy_root` after the clone's low-half frames are freed and a
/// TLB shootdown has completed.  The dead clone's higher-half subtrees are
/// never freed by `destroy_root` (they stay referenced by the parent), so this
/// decrement exactly restores `reclaim_empty_tables` to its eager behavior
/// when the last clone disappears.  Must be balanced against every
/// `register_clone_root`.
#[cfg(target_arch = "x86_64")]
pub fn unregister_clone_root(root: u64) -> bool {
    let mut clones = CLONES.lock();
    if let Some(i) = clones.iter().position(|&r| r == root) {
        clones.swap_remove(i);
        return CLONE_ROOTS.fetch_sub(1, Ordering::SeqCst) == 1;
    }
    false
}

/// Snapshot of the live cloned roots, for the clone re-sync walk.
#[cfg(target_arch = "x86_64")]
pub(crate) fn clone_roots() -> Vec<u64> {
    CLONES.lock().clone()
}

/// True when any cloned root shares the parent's higher-half subtrees.
#[cfg(target_arch = "x86_64")]
pub(crate) fn has_clone_roots() -> bool {
    CLONE_ROOTS.load(Ordering::SeqCst) != 0
}

/// Deferred page-table frames collected during an unmap and freed only after
/// the cross-CPU TLB shootdown has completed.
///
/// Reclamation of a single 4 KiB page can orphan at most three frames (the
/// PT, its parent PD, and the PDPT).  The buffer is sized for a full heap
/// chunk; `Vmm::unmap` drains it whenever it fills.
pub struct PendingFrames {
    frames: [u64; PENDING_CAPACITY],
    len: usize,
}

const PENDING_CAPACITY: usize = 256;

impl PendingFrames {
    pub fn new() -> Self {
        PendingFrames {
            frames: [0; PENDING_CAPACITY],
            len: 0,
        }
    }

    /// Record a frame to be freed once the shootdown completes.
    pub fn push(&mut self, frame: u64) {
        assert!(
            self.len < PENDING_CAPACITY,
            "VMM: pending table-frame collector overflow"
        );
        self.frames[self.len] = frame;
        self.len += 1;
    }

    pub fn remaining(&self) -> usize {
        PENDING_CAPACITY - self.len
    }

    pub fn is_full(&self) -> bool {
        self.len >= PENDING_CAPACITY
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return the deferred frames to the physical allocator.
    ///
    /// # Safety
    /// Must only be called after every CPU's TLB has been flushed of mappings
    /// referencing these frames (i.e. after `shootdown_tlb`).
    pub fn flush(&mut self, alloc: &mut BitmapAllocator) {
        for i in 0..self.len {
            unsafe {
                alloc.free(self.frames[i]);
            }
        }
        self.len = 0;
    }
}

/// Send the arch-specific TLB-shootdown IPI to one CPU.
#[inline]
fn ipi_shootdown(cpu: usize) {
    #[cfg(target_arch = "x86_64")]
    crate::platform::x86_64_pc::apic::send_ipi(
        crate::smp::per_cpu_by_id(cpu as u32).apic_id,
        crate::platform::x86_64_pc::apic::IPI_TLB_SHOOTDOWN,
    );
    #[cfg(target_arch = "riscv64")]
    crate::arch::riscv64::sbi::send_ipi(1u64 << cpu);
}

/// Re-IPI rounds before a silent target is declared unresponsive.  Round 0 is
/// the initial broadcast; rounds 1..N re-send the IPI to that CPU only (the
/// ack may have been lost to a deep spin with interrupts briefly masked).
const SHOOTDOWN_RETRY_ROUNDS: u32 = 3;

#[cfg(target_arch = "x86_64")]
const SHOOTDOWN_WINDOW_NS: u64 = 100_000_000;

/// Spin until `cpu` acknowledges `seq`, bounded by one timeout window
/// (TSC-derived when calibrated, else a fixed spin budget).
/// Returns `true` on acknowledgement.
fn shootdown_wait_window(cpu: usize, seq: u64) -> bool {
    #[cfg(target_arch = "x86_64")]
    let use_tsc = crate::platform::x86_64_pc::apic::tsc_hz() != 0;
    #[cfg(target_arch = "x86_64")]
    let deadline_ns = if use_tsc {
        crate::platform::x86_64_pc::apic::tsc_now_ns().wrapping_add(SHOOTDOWN_WINDOW_NS)
    } else {
        0
    };
    let mut spins: u64 = 0;
    while TLB_ACK[cpu].load(Ordering::Acquire) < seq {
        core::hint::spin_loop();
        #[cfg(target_arch = "x86_64")]
        {
            if use_tsc {
                if crate::platform::x86_64_pc::apic::tsc_now_ns() >= deadline_ns {
                    return false;
                }
            } else {
                spins += 1;
                if spins > 100_000_000 {
                    return false;
                }
            }
        }
        #[cfg(target_arch = "riscv64")]
        {
            spins += 1;
            if spins > 100_000_000 {
                return false;
            }
        }
    }
    true
}

/// Scoped-shootdown dispatch shared by the unmap paths: low-half-only
/// mutations target just the CPUs running `root`; anything touching the
/// shared higher half broadcasts.
fn shootdown_scoped(low_only: bool, root: u64) {
    // The scheduler publishes the next root before switch_to executes its
    // CR3 load.  A concurrent shootdown in that window would otherwise see
    // the new root in CPU_ROOT while the CPU still has the old root active,
    // skip its IPI, and permit stale low-half entries to survive frame reuse.
    // Keep the root-tracking metadata for diagnostics/future synchronization,
    // but use the conservative broadcast until the switch and shootdown
    // handshake are made atomic.
    let _ = (low_only, root);
    shootdown_tlb();
}

/// Broadcast a full TLB flush to every online CPU and wait for them all to
/// complete it.
///
/// The calling CPU must already have flushed its own TLB (`flush_tlb`).
pub fn shootdown_tlb() {
    shootdown_impl(None)
}

/// True when every page in `[vaddr, vaddr+size)` lies in the low half.
fn range_is_low_half(vaddr: u64, size: u64) -> bool {
    let end = vaddr.saturating_add(size);
    vaddr < HALF_BOUNDARY && end <= HALF_BOUNDARY
}

fn shootdown_impl(scope: Option<u64>) {
    let my = crate::smp::current_cpu_id() as usize;
    let seq = TLB_SEQ.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    // The local CPU was flushed by the caller before the seq bump.
    acknowledge_tlb(seq);

    // Resolve the target set once so send and wait phases agree exactly.
    // Send to online CPUs only — a starting AP has no IDT to handle this IPI,
    // and reclaim cannot run before all APs are online anyway.
    let mut targets: u16 = 0;
    for cpu in 0..crate::smp::MAX_CPUS {
        if cpu == my {
            continue;
        }
        if !crate::smp::is_cpu_online(cpu as u32) {
            continue;
        }
        if let Some(root) = scope {
            if CPU_ROOT[cpu].load(Ordering::Relaxed) != root {
                continue;
            }
        }
        targets |= 1u16 << cpu;
        ipi_shootdown(cpu);
    }

    // Wait for every targeted CPU to flush and acknowledge, re-IPIing
    // stragglers up to SHOOTDOWN_RETRY_ROUNDS times.
    let mut cpu = 0usize;
    while targets != 0 {
        if targets & 1 == 1 {
            let mut acked = false;
            for round in 1..SHOOTDOWN_RETRY_ROUNDS {
                if shootdown_wait_window(cpu, seq) {
                    acked = true;
                    break;
                }
                crate::drivers::serial::SerialPort::puts("[vmm] WARN: shootdown ack timeout cpu=");
                crate::drivers::serial::SerialPort::put_u64(cpu as u64);
                crate::drivers::serial::SerialPort::puts(" seq=");
                crate::drivers::serial::SerialPort::put_u64(seq);
                crate::drivers::serial::SerialPort::puts(" — re-IPI round ");
                crate::drivers::serial::SerialPort::put_u64(round as u64 + 1);
                crate::drivers::serial::SerialPort::puts("/");
                crate::drivers::serial::SerialPort::put_u64(SHOOTDOWN_RETRY_ROUNDS as u64);
                crate::drivers::serial::SerialPort::puts("\n");
                ipi_shootdown(cpu);
            }
            if !acked && shootdown_wait_window(cpu, seq) {
                acked = true;
            }
            if !acked {
                crate::drivers::serial::SerialPort::puts("[vmm] FATAL: cpu=");
                crate::drivers::serial::SerialPort::put_u64(cpu as u64);
                crate::drivers::serial::SerialPort::puts(" never acknowledged shootdown seq=");
                crate::drivers::serial::SerialPort::put_u64(seq);
                crate::drivers::serial::SerialPort::puts(" — stale-TLB frame reuse risk\n");
                #[cfg(target_arch = "x86_64")]
                crate::kerneldump::dump_fatal("TLB shootdown: target CPU unresponsive");
                #[cfg(target_arch = "riscv64")]
                loop {
                    unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
                }
            }
        }
        targets >>= 1;
        cpu += 1;
    }
}

/// Flush this CPU's TLB and acknowledge the latest shootdown generation.
///
/// Called from the TLB-shootdown IPI handler (vector 241 on x86_64, the SBI
/// software-IPI branch on riscv64) on every online CPU.
pub fn tlb_shootdown_on_this_cpu() {
    flush_tlb();
    let seq = TLB_SEQ.load(Ordering::Acquire);
    acknowledge_tlb(seq);
}

// ── VirtualMemoryManager provider ─────────────────────────────────────
//
// Implemented now that `Vmm` stores its own allocator pointer. The inherent
// `map`/`unmap` methods (which take an explicit `&mut BitmapAllocator`) are
// kept for early-boot/explicit callers; the provider methods reuse the
// stored allocator.
use crate::services::virt_mem::VirtualMemoryManager;

impl VirtualMemoryManager for Vmm {
    fn map(&mut self, vaddr: u64, paddr: u64, size: u64, flags: PageFlags) {
        let alloc = self.alloc;
        assert!(!alloc.is_null(), "VMM::map: no allocator bound to Vmm");
        self.map(unsafe { &mut *alloc }, vaddr, paddr, size, flags);
    }

    fn unmap(&mut self, vaddr: u64, size: u64) {
        let alloc = self.alloc;
        assert!(!alloc.is_null(), "VMM::unmap: no allocator bound to Vmm");
        self.unmap(unsafe { &mut *alloc }, vaddr, size);
    }

    fn translate(&self, vaddr: u64) -> Option<u64> {
        self.translate(vaddr)
    }

    fn root(&self) -> u64 {
        self.root
    }

    fn flush_tlb(&self) {
        self.flush_tlb()
    }
}
