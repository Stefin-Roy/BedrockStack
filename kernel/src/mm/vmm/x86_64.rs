//! x86_64 4-level page table operations.
//!
//! Wraps the `x86_64` crate's `OffsetPageTable`.  Before the DIRECT_MAP
//! physmap is enabled the offset is 0 (identity); once `init_physmap` runs the
//! offset becomes `PHYS_MAP_BASE`, so page-table frames are dereferenced
//! through the kernel-internal physmap rather than the identity map.

use alloc::vec::Vec;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size2MiB,
    Size4KiB, Translate,
};
use x86_64::{PhysAddr as XPhysAddr, VirtAddr};

use super::PageFlags;
use crate::mm::phys_alloc::BitmapAllocator;

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
    let off = crate::mm::layout::phys_offset();
    let root_ptr = root.wrapping_add(off) as *mut PageTable;
    unsafe { OffsetPageTable::new(&mut *root_ptr, VirtAddr::new(off)) }
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
        // Need BOTH PCD and PWT for UC (PAT index 3 = 0 × 4 + 2 × 1 + 1 × 1).
        // With only PCD (bit 4), PAT index = 2 → WT (Write-Through),
        // which allows cache hits on reads giving stale data.
        f |= PageTableFlags::NO_CACHE; // PCD
        f |= PageTableFlags::WRITE_THROUGH; // PWT
    }
    if flags.contains(PageFlags::WRITE_COMBINING) {
        // PAT index 1 (001): PWT=1, PCD=0, PAT=0
        // Requires PAT MSR entry 1 = 01h (WC).
        f |= PageTableFlags::WRITE_THROUGH;
    }
    if flags.contains(PageFlags::USER) {
        f |= PageTableFlags::USER_ACCESSIBLE;
    }
    f
}

/// Program IA32_PAT MSR so that PAT entry 1 = WC (01h).
pub fn init_pat_wc() {
    use x86_64::registers::model_specific::Msr;
    const IA32_PAT: u32 = 0x277;
    let mut msr = Msr::new(IA32_PAT);
    let val = unsafe { msr.read() };
    // Entry 1 is bits 15:8. Change byte 1 to 01h (WC).
    let new_val = (val & !(0xFF << 8)) | (0x01u64 << 8);
    unsafe {
        msr.write(new_val);
    }
}

/// Table flags for newly-allocated intermediate page-table entries.
///
/// Propagates the USER_ACCESSIBLE bit from the leaf being mapped so that user
/// mappings stay reachable through every level.  Invariant: user mappings must
/// live under freshly-created intermediate tables — they do, because the low
/// half of a `clone_high_half` root starts empty and every table on the path is
/// allocated by `map_to_with_table_flags`, which only applies these flags to
/// new tables.  (Kernel leaves stay as before: no USER on their tables.)
#[inline]
fn table_flags_for(leaf: PageTableFlags) -> PageTableFlags {
    let mut f = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::ACCESSED;
    if leaf.contains(PageTableFlags::USER_ACCESSIBLE) {
        f |= PageTableFlags::USER_ACCESSIBLE;
    }
    f
}

// ── Public API ──────────────────────────────────────────────────────

pub fn map_4k(root: u64, alloc: &mut BitmapAllocator, vaddr: u64, paddr: u64, flags: PageFlags) {
    // Table mutation + clone fan-out happen under the VMM lock; the (possibly
    // long) shootdown wait runs after the guard is dropped so other CPUs can
    // proceed with their own VMM work instead of serializing behind this map.
    let new_slot = {
        let _guard = super::lock();
        let new_slot = unsafe { new_higher_half_slot(root, vaddr) };
        let mut mapper = mapper_at(root);
        let mut frame_alloc = BitmapFrameAllocator { inner: alloc };

        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
        let frame = PhysFrame::<Size4KiB>::containing_address(XPhysAddr::new(paddr));
        let x86_flags = page_flags_to_x86(flags);

        unsafe {
            mapper
                .map_to_with_table_flags(
                    page,
                    frame,
                    x86_flags,
                    table_flags_for(x86_flags),
                    &mut frame_alloc,
                )
                .expect("x86_64 4KiB map failed")
                .flush();
        }
        if new_slot {
            sync_clone_half(root);
        }
        new_slot
    };
    if new_slot {
        super::flush_tlb();
        super::shootdown_tlb();
    }
}

pub fn map_2m(root: u64, alloc: &mut BitmapAllocator, vaddr: u64, paddr: u64, flags: PageFlags) {
    let new_slot = {
        let _guard = super::lock();
        let new_slot = unsafe { new_higher_half_slot(root, vaddr) };
        let mut mapper = mapper_at(root);
        let mut frame_alloc = BitmapFrameAllocator { inner: alloc };

        let page = Page::<Size2MiB>::containing_address(VirtAddr::new(vaddr));
        let frame = PhysFrame::<Size2MiB>::containing_address(XPhysAddr::new(paddr));
        let x86_flags = page_flags_to_x86(flags);

        unsafe {
            mapper
                .map_to_with_table_flags(
                    page,
                    frame,
                    x86_flags,
                    table_flags_for(x86_flags),
                    &mut frame_alloc,
                )
                .expect("x86_64 2MiB map failed")
                .flush();
        }
        if new_slot {
            sync_clone_half(root);
        }
        new_slot
    };
    if new_slot {
        super::flush_tlb();
        super::shootdown_tlb();
    }
}

pub fn unmap_4k(root: u64, vaddr: u64, pending: &mut super::PendingFrames) -> bool {
    if !unmap_4k_impl(root, vaddr) {
        return false;
    }
    reclaim_empty_tables(root, pending, vaddr);
    true
}

/// Clear the leaf PTE at `vaddr` without reclaiming intermediate tables.
///
/// Used by range-unmap paths that batch reclamation once per touched page
/// table instead of per page (the range loop must then run
/// [`reclaim_empty_tables`] per distinct group before the shootdown).
pub fn unmap_4k_no_reclaim(root: u64, vaddr: u64, _pending: &mut super::PendingFrames) -> bool {
    unmap_4k_impl(root, vaddr)
}

fn unmap_4k_impl(root: u64, vaddr: u64) -> bool {
    let _guard = super::lock();
    let mut mapper = mapper_at(root);

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let removed = match mapper.unmap(page) {
        Ok((_mapped_frame, flush)) => {
            // Local per-page invalidation only.  The range-level path performs
            // the full local flush plus a cross-CPU TLB shootdown before any
            // frame (leaf or intermediate table) is released to the allocator.
            flush.flush();
            // NB: `_mapped_frame` is the frame that was mapped at this VA and
            // is owned by the *caller* — freeing it here would release memory
            // another component may still be using.  We reclaim only the
            // intermediate (L1/L2/L3) tables, once they become empty.
            true
        }
        Err(_) => false,
    };
    removed
}

/// The low 12 flag bits of an x86_64 PTE; bit 0 is PRESENT.
const PTE_PRESENT: u64 = 1 << 0;

/// Physical-frame bits of a page-table entry (bits 12..=51).
fn pte_frame(entry: u64) -> u64 {
    entry & 0x000F_FFFF_FFFF_F000
}

/// Virtual deref address for a page-table frame (through the private physmap).
fn pte_deref(frame: u64) -> *mut u64 {
    (crate::mm::layout::to_physmap(frame)) as *mut u64
}

#[inline]
unsafe fn read_pte(table: *mut u64, index: usize) -> u64 {
    unsafe { *table.add(index & 0x1FF) }
}

#[inline]
unsafe fn write_pte(table: *mut u64, index: usize, value: u64) {
    unsafe {
        *table.add(index & 0x1FF) = value;
    }
}

/// True if every one of the 512 entries in the table at physical `frame` is
/// non-present (i.e. the table holds no leaf and points to no deeper level).
unsafe fn table_is_empty(frame: u64) -> bool {
    let table = pte_deref(frame);
    for i in 0..512 {
        if unsafe { read_pte(table, i) } & PTE_PRESENT != 0 {
            return false;
        }
    }
    true
}

/// Copy the parent's higher-half PML4 entries (256..=511) into every live
/// clone, so a higher-half PML4 slot that was *absent* when a clone was born —
/// and is now populated on the parent — becomes visible to it.
///
/// Clones share the parent's subtrees for entries that were present at clone
/// time; only entries that were empty then need syncing.  The parent is
/// normally the kernel root, but `root` is taken as a parameter so any root's
/// new higher-half slot can fan out.
///
/// # Safety/ordering
/// Must be called with the VMM operation lock held (as from `map_4k` /
/// `map_2m`), and the caller must follow up with a local `flush_tlb` plus a
/// cross-CPU `shootdown_tlb` before any affected frames could be re-used.
pub(crate) fn sync_clone_half(parent_root: u64) {
    let clones = super::clone_roots();
    if clones.is_empty() {
        return;
    }
    unsafe {
        let parent = pte_deref(parent_root);
        for root in clones {
            let pml4 = pte_deref(root);
            for i in 256..=511usize {
                let pe = read_pte(parent, i);
                if read_pte(pml4, i) != pe {
                    write_pte(pml4, i, pe);
                }
            }
        }
    }
}

/// True when `vaddr` is a higher-half address whose PML4 slot is *absent* in
/// `root` right now — i.e. mapping it introduces a brand-new higher-half slot
/// that every clone must be re-synced against.
unsafe fn new_higher_half_slot(root: u64, vaddr: u64) -> bool {
    let i = ((vaddr >> 39) & 0x1FF) as usize;
    if i < 256 {
        return false;
    }
    let pml4 = pte_deref(root);
    unsafe { read_pte(pml4, i) & PTE_PRESENT == 0 }
}

/// Free 4-level intermediate page tables that became empty after the leaf at
/// `vaddr` was unmapped, returning their frames to the allocator.
///
/// Walks from the PML4 down to the deepest (Level-1) table holding the leaf.
/// After the leaf is cleared (already done by the caller), each table is freed
/// and its parent entry cleared only when it has no remaining present entries.
/// The root (PML4) itself is never freed — it is owned by the kernel.
///
/// Orphaned frames are pushed into `pending` rather than freed directly: the
/// caller completes a cross-CPU TLB shootdown before `pending.flush()` returns
/// them to the allocator, so no CPU re-walks a frame that is being released.
///
/// # Clone subtrees
/// `clone_high_half` shares the parent's higher-half PDPT/PD/PT subtrees into
/// every cloned root.  Those tables are still walked by the clone even after
/// this root drops them, so when any clone exists the empty tables are left
/// allocated (their parent entries are still cleared, releasing the VA space)
/// rather than freed — freeing a shared table would let the clone's next walk
/// dereference a reallocated frame.
pub(super) fn reclaim_empty_tables(root: u64, pending: &mut super::PendingFrames, vaddr: u64) {
    // Guarded by caller `reclaim_tables` (returns early when clones live) and `keep_frames` below.
    // No debug_assert that panics with `panic=abort` here — clones are normal during process lifetime.
    // Level-0 (leaf) table index, Level-1 (PD), Level-2 (PDPT), Level-3 (PML4).
    #[rustfmt::skip]
    let (i_pt, i_pd, i_pdpt, i_pml4) = (
        ((vaddr >> 12) & 0x1FF) as usize,
        ((vaddr >> 21) & 0x1FF) as usize,
        ((vaddr >> 30) & 0x1FF) as usize,
        ((vaddr >> 39) & 0x1FF) as usize,
    );

    let keep_frames = super::has_clone_roots();

    unsafe {
        let pml4 = pte_deref(root);
        let pml4e = read_pte(pml4, i_pml4);
        if pml4e & PTE_PRESENT == 0 {
            return;
        }
        let pdpt = pte_deref(pte_frame(pml4e));
        let pdpte = read_pte(pdpt, i_pdpt);
        if pdpte & PTE_PRESENT == 0 {
            return;
        }
        let pd = pte_deref(pte_frame(pdpte));
        let pde = read_pte(pd, i_pd);
        if pde & PTE_PRESENT == 0 {
            return;
        }
        // This must be a non-leaf entry pointing at the Level-1 table if the
        // map was created with 2M huge pages this entry is a leaf — nothing to
        // reclaim (the unmapped page was a 4K leaf deeper down).
        let pt_frame = pte_frame(pde);
        let pt = pte_deref(pt_frame);
        if read_pte(pt, i_pt) & PTE_PRESENT != 0 {
            return; // line already re-mapped; leave as-is
        }

        if table_is_empty(pt_frame) {
            if !keep_frames {
                pending.push(pt_frame);
            }
            write_pte(pd, i_pd, 0);
            // PD may now be empty → free it and clear PDPT entry.
            let pd_frame = pte_frame(pdpte);
            if table_is_empty(pd_frame) {
                if !keep_frames {
                    pending.push(pd_frame);
                }
                write_pte(pdpt, i_pdpt, 0);
                let pdpt_frame = pte_frame(pml4e);
                if table_is_empty(pdpt_frame) {
                    if !keep_frames {
                        pending.push(pdpt_frame);
                    }
                    write_pte(pml4, i_pml4, 0);
                }
            }
        }
    }
}

pub fn translate(root: u64, vaddr: u64) -> Option<u64> {
    let mapper = mapper_at(root);
    mapper
        .translate_addr(VirtAddr::new(vaddr))
        .map(|p| p.as_u64())
}

// ── Last-clone empty-table sweep ──────────────────────────────────────

const PTE_PS: u64 = 1 << 7;

/// Recover every fully-empty intermediate table under `root`'s higher-half
/// PML4 slots (256..=511), clearing the entries that referenced them.
///
/// While any clone lives, `reclaim_empty_tables` deliberately *leaks* tables
/// that become empty (clones alias the parent's subtrees by reference, so
/// freeing one could hand its frame to a new owner mid-walk).  This sweep
/// runs exactly once — when the last clone unregisters — so every shared
/// walker is gone and emptiness implies safety.  A future mapping simply
/// re-allocates the table.
///
/// Returns the number of frames pushed into `pending`; the caller must run a
/// full flush + [`super::shootdown_tlb`] before `pending.flush`.
///
/// # Locking
/// Caller must hold [`super::lock`].
pub(super) fn sweep_empty_higher_half_tables(
    root: u64,
    pending: &mut Vec<u64>,
) -> usize {
    let mut freed = 0;
    unsafe {
        let pml4 = pte_deref(root);
        for i in 256..=511usize {
            let e = read_pte(pml4, i);
            if e & PTE_PRESENT == 0 || e & PTE_PS != 0 {
                // Absent slot, or a 1 GiB leaf: nothing to walk.
                continue;
            }
            let pdpt_frame = pte_frame(e);
            if sweep_reclaim(pdpt_frame, 2, pending) {
                write_pte(pml4, i, 0);
                pending.push(pdpt_frame);
                freed += 1;
            }
        }
    }
    freed
}

/// Recursive emptiness reclamation below one PML4 slot.
///
/// `level` 2 = PDPT (children are PDs, which own PT leaves),
/// `level` 1 = PD (children are PTs — the bottom table tier).
/// Returns `true` when the table itself became empty (caller frees it).
///
/// # Locking
/// Caller must hold [`super::lock`].
fn sweep_reclaim(tbl_frame: u64, level: u8, pending: &mut Vec<u64>) -> bool {
    unsafe {
        let tbl = pte_deref(tbl_frame);
        let mut empty = true;
        for i in 0..512usize {
            let e = read_pte(tbl, i);
            if e & PTE_PRESENT == 0 {
                continue;
            }
            if e & PTE_PS != 0 {
                // Huge page leaf (2 MiB here): real content, stop.
                empty = false;
                break;
            }
            let child = pte_frame(e);
            if level <= 1 {
                // Child is a PT — bottom tier, emptiness decided directly.
                if table_is_empty(child) {
                    write_pte(tbl, i, 0);
                    pending.push(child);
                } else {
                    empty = false;
                }
            } else {
                if sweep_reclaim(child, level - 1, pending) {
                    write_pte(tbl, i, 0);
                    pending.push(child);
                } else {
                    empty = false;
                }
            }
        }
        empty
    }
}

/// Manual 4-level page-table walk returning `(physical, is_user, is_writable)`.
///
/// Unlike [`translate`], this reports permissions too, so the syscall layer can
/// reject non-user pointers before dereferencing them (there is no #PF handler
/// for a bogus user buffer — a raw copy would abort the kernel).
///
/// Every level must be PRESENT; U/S is checked on all four levels because
/// `table_flags_for` propagates USER_ACCESSIBLE onto freshly allocated
/// intermediate tables, and a mapping only reaches user mode if every level
/// carries the bit.  W is read from the leaf.  Huge (PS) pages at levels 3 and
/// 2 are handled.  Returns `None` when any level is not present.
pub fn translate_user(root: u64, vaddr: u64) -> Option<(u64, bool, bool)> {
    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
    const PS: u64 = 1 << 7;

    let i0 = ((vaddr >> 39) & 0x1FF) as usize;
    let i1 = ((vaddr >> 30) & 0x1FF) as usize;
    let i2 = ((vaddr >> 21) & 0x1FF) as usize;
    let i3 = ((vaddr >> 12) & 0x1FF) as usize;

    let pml4 = pte_deref(root);
    let e0 = unsafe { read_pte(pml4, i0) };
    if e0 & PRESENT == 0 {
        return None;
    }
    let mut user_ok = e0 & USER != 0;

    let pdpt = pte_deref(pte_frame(e0));
    let e1 = unsafe { read_pte(pdpt, i1) };
    if e1 & PRESENT == 0 {
        return None;
    }
    user_ok &= e1 & USER != 0;
    if e1 & PS != 0 {
        return Some((
            pte_frame(e1) | (vaddr & 0x3FFF_FFFF),
            user_ok,
            e1 & WRITABLE != 0,
        ));
    }

    let pd = pte_deref(pte_frame(e1));
    let e2 = unsafe { read_pte(pd, i2) };
    if e2 & PRESENT == 0 {
        return None;
    }
    user_ok &= e2 & USER != 0;
    if e2 & PS != 0 {
        return Some((
            pte_frame(e2) | (vaddr & 0x1F_FFFF),
            user_ok,
            e2 & WRITABLE != 0,
        ));
    }

    let pt = pte_deref(pte_frame(e2));
    let e3 = unsafe { read_pte(pt, i3) };
    if e3 & PRESENT == 0 {
        return None;
    }
    Some((
        pte_frame(e3) | (vaddr & 0xFFF),
        user_ok & (e3 & USER != 0),
        e3 & WRITABLE != 0,
    ))
}

/// Batched permission check for a contiguous user VA range — caches the
/// upper-level table entries across consecutive pages so a multi-megabyte
/// buffer (e.g. a `/dev/fb` blit) costs ~1 physmap load per page instead of
/// 4. Huge pages are handled; permission must hold on every page.
pub fn translate_user_range_ok(root: u64, ptr: u64, len: u64, need_writable: bool) -> bool {
    if len == 0 {
        return true;
    }
    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
    const PS: u64 = 1 << 7;
    let mut va = ptr & !0xFFF;
    let pages = (((ptr & 0xFFF) + len + 0xFFF) >> 12) as usize;
    let mut cached: Option<(usize, usize, usize, u64, u64, u64, bool)> = None;
    // (i0, i1, i2, e0, e1, e2, have_e2)
    for _ in 0..pages {
        let i0 = ((va >> 39) & 0x1FF) as usize;
        let i1 = ((va >> 30) & 0x1FF) as usize;
        let i2 = ((va >> 21) & 0x1FF) as usize;
        let i3 = ((va >> 12) & 0x1FF) as usize;
        let (e0, e1, e2, have_e2, leaf_writable, leaf_user) = if let Some((ci0, ci1, ci2, ce0, ce1, ce2, ch)) = cached {
            if ci0 == i0 && ci1 == i1 && ci2 == i2 {
                // Same PD/PT as previous page — leaf only.
                let writable: bool;
                let user_ok: bool;
                if ce1 & PS != 0 {
                    writable = ce1 & WRITABLE != 0;
                    user_ok = true; // user already validated at cached levels
                } else if ch && (ce2 & PS != 0) {
                    writable = ce2 & WRITABLE != 0;
                    user_ok = true;
                } else {
                    // Need PT leaf for this i3.
                    let pt = pte_deref(pte_frame(ce2));
                    let e3 = unsafe { read_pte(pt, i3) };
                    if e3 & PRESENT == 0 {
                        return false;
                    }
                    writable = e3 & WRITABLE != 0;
                    user_ok = e3 & USER != 0;
                }
                if need_writable && !writable {
                    return false;
                }
                if !user_ok {
                    return false;
                }
                va = va.wrapping_add(0x1000);
                continue;
            }
            (ce0, ce1, ce2, ch, false, false)
        } else {
            (0, 0, 0, false, false, false)
        };
        // Full walk for this page, then cache upper entries.
        let pml4 = pte_deref(root);
        let ne0 = unsafe { read_pte(pml4, i0) };
        if ne0 & PRESENT == 0 || ne0 & USER == 0 {
            return false;
        }
        let pdpt = pte_deref(pte_frame(ne0));
        let ne1 = unsafe { read_pte(pdpt, i1) };
        if ne1 & PRESENT == 0 || ne1 & USER == 0 {
            return false;
        }
        if ne1 & PS != 0 {
            if need_writable && ne1 & WRITABLE == 0 {
                return false;
            }
            cached = Some((i0, i1, i2, ne0, ne1, 0, false));
            va = va.wrapping_add(0x1000);
            continue;
        }
        let pd = pte_deref(pte_frame(ne1));
        let ne2 = unsafe { read_pte(pd, i2) };
        if ne2 & PRESENT == 0 || ne2 & USER == 0 {
            return false;
        }
        if ne2 & PS != 0 {
            if need_writable && ne2 & WRITABLE == 0 {
                return false;
            }
            cached = Some((i0, i1, i2, ne0, ne1, ne2, true));
            va = va.wrapping_add(0x1000);
            continue;
        }
        let pt = pte_deref(pte_frame(ne2));
        let e3 = unsafe { read_pte(pt, i3) };
        if e3 & PRESENT == 0 || e3 & USER == 0 {
            return false;
        }
        if need_writable && e3 & WRITABLE == 0 {
            return false;
        }
        cached = Some((i0, i1, i2, ne0, ne1, ne2, true));
        let _ = (e0, e1, e2, have_e2, leaf_writable, leaf_user);
        va = va.wrapping_add(0x1000);
    }
    true
}

/// PS (huge-page) bit position in a level-2/3 entry.
const PS: u64 = 1 << 7;

/// Destroy a cloned root's user-space (low-half) mappings wholesale, freeing
/// every leaf frame and empty intermediate table, then the root PML4 itself.
/// The caller's count of live clones is decremented (`unregister_clone_root`).
///
/// # Safety
/// Safe only for a root no CPU is running: the scheduler is BSP-only and a
/// parked `Dead` task's root is idle, so no TLB anywhere can re-walk a frame
/// we release — the flush (local + broadcast) that precedes the frees is
/// strictly defensive.  The higher-half entries (PML4 indices 256..=511) are
/// never dereferenced or freed: they alias the kernel root's shared subtrees,
/// which are owned by the parent and outlive every clone.
///
/// Leaf freeing is valid here because a clone root's low half is entirely
/// user-owned: `clone_high_half` copies only the higher half, and `create_process`
/// allocates every low-half frame it maps, so nothing kernel-owned can be
/// collateral.
///
/// Frames are collected into a heap `Vec` (unbounded — a whole process address
/// space can exceed the fixed `PendingFrames` collector) and released only
/// after a local `flush_tlb` plus a cross-CPU `shootdown_tlb`, so no CPU can
/// dereference a released frame.  The VMM lock is held only for the table-walk
/// mutation phase, never across the shootdown wait.
pub fn destroy_root(root: u64, alloc: &mut BitmapAllocator) {
    let mut frames: Vec<u64> = Vec::new();

    {
        let _guard = super::lock();
        unsafe {
            let pml4 = pte_deref(root);
            for i0 in 0..256usize {
                let e0 = read_pte(pml4, i0);
                if e0 & PTE_PRESENT == 0 {
                    continue;
                }
                let pdpt_base = pte_frame(e0);
                let pdpt = pte_deref(pdpt_base);
                for i1 in 0..512usize {
                    let e1 = read_pte(pdpt, i1);
                    if e1 & PTE_PRESENT == 0 {
                        continue;
                    }
                    if e1 & PS != 0 {
                        // 1 GiB huge leaf (defensive — user maps use 4 KiB).
                        frames.push(pte_frame(e1));
                        write_pte(pdpt, i1, 0);
                        continue;
                    }
                    let pd_base = pte_frame(e1);
                    let pd = pte_deref(pd_base);
                    for i2 in 0..512usize {
                        let e2 = read_pte(pd, i2);
                        if e2 & PTE_PRESENT == 0 {
                            continue;
                        }
                        if e2 & PS != 0 {
                            // 2 MiB huge leaf (defensive).
                            frames.push(pte_frame(e2));
                            write_pte(pd, i2, 0);
                            continue;
                        }
                        let pt_base = pte_frame(e2);
                        let pt = pte_deref(pt_base);
                        for i3 in 0..512usize {
                            let e3 = read_pte(pt, i3);
                            if e3 & PTE_PRESENT == 0 {
                                continue;
                            }
                            frames.push(pte_frame(e3));
                            write_pte(pt, i3, 0);
                        }
                        frames.push(pt_base);
                        write_pte(pd, i2, 0);
                    }
                    frames.push(pd_base);
                    write_pte(pdpt, i1, 0);
                }
                frames.push(pdpt_base);
                write_pte(pml4, i0, 0);
            }
        }
    }

    super::flush_tlb();
    super::shootdown_tlb();
    // Refcount-aware release: leaf frames shared with fork siblings only
    // drop one reference here; intermediate tables and untracked frames are
    // released exactly as before (`framecnt` treats entry-0 frames as
    // single-owner and frees them outright).
    for f in frames {
        crate::mm::framecnt::decref_or_free(alloc, f);
    }
    let was_last = super::unregister_clone_root(root);
    if was_last {
        // Last clone gone: recover the parent's higher-half tables that
        // `reclaim_empty_tables` deliberately leaked while clones shared them.
        // Collect under the VMM lock, shootdown, then free — same pattern as
        // the low-half walk above.
        let parent = crate::task::kernel_root();
        let mut pending: Vec<u64> = Vec::new();
        {
            let _guard = super::lock();
            sweep_empty_higher_half_tables(parent, &mut pending);
        }
        if !pending.is_empty() {
            super::flush_tlb();
            super::shootdown_tlb();
            for frame in pending {
                unsafe { alloc.free(frame); }
            }
        }
    }

    // The root PML4 itself is owned by this task and was intentionally not
    // reachable from the low-half walk above.  Return it after the final
    // shootdown; otherwise every process permanently leaks one physical
    // frame even though all of its mappings have been destroyed.
    unsafe { alloc.free(root); }
}

/// Remove the WRITABLE flag from a single 4 KiB page, making it read-only.
///
/// Phase 4: the kernel image is mapped exactly once, at `KERNEL_VMA` — there
/// is no separate identity alias any more — so this operates on the single VA
/// passed in.  `protect_idt` passes the `.idt` section's high VMA directly.
///
/// The page must already be mapped with 4 KiB granularity via 4-level paging
/// (the kernel image is mapped 4 KiB-per-page).
/// Panics if the page is not present.
pub fn make_read_only(root: u64, vaddr: u64) {
    // PTE mutation must hold the global VMM lock like every other mutator
    // (this was previously unlocked — a latent race against concurrent maps).
    let _guard = super::lock();
    let mut mapper = mapper_at(root);
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    // Reconstruct flags matching what leaf_flags() would have set for
    // .data/.bss (READ | WRITE | NO_EXECUTE) minus WRITABLE.
    let flags = PageTableFlags::PRESENT | PageTableFlags::ACCESSED | PageTableFlags::NO_EXECUTE;
    unsafe {
        mapper
            .update_flags(page, flags)
            .expect("make_read_only: update_flags failed")
            .flush();
    }
}

// ── User leaf flag editing (CoW / protection support) ────────────────

const PAGE_SIZE: u64 = 4096;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER: u64 = 1 << 2;
const PTE_ACCESSED: u64 = 1 << 5;

#[inline]
unsafe fn invlg(va: u64) {
    // SAFETY: `va` must be a currently-mapped canonical VA — guaranteed by
    // callers, which only invalidate leaves they just walked as PRESENT.
    unsafe { core::arch::asm!("invlpg [{va}]", va = in(reg) va, options(nostack, nomem)) };
}

/// Walk to the leaf PTE slot of a 4 KiB mapping without allocating.
///
/// Returns `(table_ptr, index)` for the PT-level entry. Every level must be
/// PRESENT and non-huge — user mappings are always created with `map_4k`, so
/// a PS leaf anywhere above the PT means the VA is not a user 4 KiB mapping
/// and the caller must not touch it.
///
/// # Locking
/// Caller must hold [`super::lock`] (the pointer is only valid under it).
fn walk_leaf(root: u64, vaddr: u64) -> Option<(*mut u64, usize)> {
    let i0 = ((vaddr >> 39) & 0x1FF) as usize;
    let i1 = ((vaddr >> 30) & 0x1FF) as usize;
    let i2 = ((vaddr >> 21) & 0x1FF) as usize;
    let i3 = ((vaddr >> 12) & 0x1FF) as usize;
    unsafe {
        let pml4 = pte_deref(root);
        let e0 = read_pte(pml4, i0);
        if e0 & PTE_PRESENT == 0 || e0 & PTE_PS != 0 {
            return None;
        }
        let pdpt = pte_deref(pte_frame(e0));
        let e1 = read_pte(pdpt, i1);
        if e1 & PTE_PRESENT == 0 || e1 & PTE_PS != 0 {
            return None;
        }
        let pd = pte_deref(pte_frame(e1));
        let e2 = read_pte(pd, i2);
        if e2 & PTE_PRESENT == 0 || e2 & PTE_PS != 0 {
            return None;
        }
        Some((pte_deref(pte_frame(e2)), i3))
    }
}

/// Read-modify-write the raw leaf PTE of one 4 KiB mapping via `f`.
///
/// The callback receives the full entry (frame bits, PAT/AVL software bits
/// included) and returns the replacement — preserve everything you do not
/// mean to change. Returns `true` when a present page was found *and* the
/// entry changed; the local TLB entry is invalidated in that case.
///
/// Other CPUs that may have this root active are NOT handled here: follow up
/// with [`super::shootdown_tlb_for_root`] when the root is not exclusively
/// running on this CPU.
pub fn edit_user_leaf(root: u64, vaddr: u64, f: impl FnOnce(u64) -> u64) -> bool {
    let _guard = super::lock();
    let Some((pt, idx)) = walk_leaf(root, vaddr) else {
        return false;
    };
    unsafe {
        let old = read_pte(pt, idx);
        let new = f(old);
        if new == old {
            return false;
        }
        write_pte(pt, idx, new);
        invlg(vaddr & !0xFFF);
    }
    true
}

/// Batched [`edit_user_leaf`] over `[vaddr, vaddr + pages*PAGE)`.
///
/// One VMM lock hold and one local full-flush for the whole range; returns
/// the number of entries changed. Follow with a scoped shootdown for `root`
/// before relying on other CPUs observing the new permissions.
pub fn edit_user_leaf_range(
    root: u64,
    vaddr: u64,
    pages: u64,
    f: impl Fn(u64) -> u64 + Copy,
) -> usize {
    let _guard = super::lock();
    let mut changed = 0usize;
    for p in 0..pages {
        let va = vaddr + p * PAGE_SIZE;
        let Some((pt, idx)) = walk_leaf(root, va) else {
            continue;
        };
        unsafe {
            let old = read_pte(pt, idx);
            let new = f(old);
            if new != old {
                write_pte(pt, idx, new);
                changed += 1;
            }
        }
    }
    if changed > 0 {
        super::flush_tlb();
    }
    changed
}

/// Clear WRITABLE on one present 4 KiB user leaf, preserving every other bit.
pub fn user_leaf_write_protect(root: u64, vaddr: u64) -> bool {
    edit_user_leaf(root, vaddr, |e| {
        if e & PTE_PRESENT == 0 {
            e
        } else {
            e & !PTE_WRITABLE
        }
    })
}

/// Batched write-protect sweep used by fork; skips absent pages. Returns the
/// number of entries actually downgraded.
pub fn user_leaf_write_protect_range(root: u64, vaddr: u64, pages: u64) -> usize {
    edit_user_leaf_range(root, vaddr, pages, |e| {
        if e & PTE_PRESENT == 0 {
            e
        } else {
            e & !PTE_WRITABLE
        }
    })
}

/// Restore WRITABLE (keeping ACCESSED set so the hardware doesn't refault the
/// A-bit needlessly) on one present 4 KiB user leaf.
pub fn user_leaf_make_writable(root: u64, vaddr: u64) -> bool {
    edit_user_leaf(root, vaddr, |e| {
        if e & PTE_PRESENT == 0 {
            e
        } else {
            e | PTE_WRITABLE | PTE_ACCESSED
        }
    })
}

/// Restore WRITABLE on every present 4 KiB leaf in `[vaddr, vaddr+pages*PAGE)`.
/// Batched sibling of [`user_leaf_make_writable`] — used by the fork failure
/// path to roll the parent's write-downgrade sweep back in one lock hold.
pub fn user_leaf_make_writable_range(root: u64, vaddr: u64, pages: u64) -> usize {
    edit_user_leaf_range(root, vaddr, pages, |e| {
        if e & PTE_PRESENT == 0 {
            e
        } else {
            e | PTE_WRITABLE | PTE_ACCESSED
        }
    })
}

/// Point an existing present 4 KiB leaf at `new_paddr`, restoring WRITABLE.
///
/// Low flag bits (USER, NX, PAT/PCD/PWT, DIRTY) are preserved from the old
/// entry; only the frame bits change. Used by the CoW resolver to install a
/// private copy.
pub fn user_leaf_repoint_writable(root: u64, vaddr: u64, new_paddr: u64) -> bool {
    let np = new_paddr & !0xFFF;
    edit_user_leaf(root, vaddr, |e| {
        (e & 0xFFF) | np | PTE_PRESENT | PTE_WRITABLE | PTE_ACCESSED
    })
}

/// PTE protection-key field: bits 59..62 (PKU).
const PTE_PKEY_SHIFT: u64 = 59;

/// Tag every present 4 KiB leaf in `[vaddr, vaddr+pages*PAGE)` with
/// protection key `key` (0..=15). Absent pages are skipped — lazy fills and
/// COW copies pick the key up from region metadata when they materialize.
/// Returns the number of entries changed; follow with a shootdown if the
/// root may be active on other CPUs.
pub fn user_leaf_set_pkey_range(root: u64, vaddr: u64, pages: u64, key: u8) -> usize {
    let k = ((key & 0xF) as u64) << PTE_PKEY_SHIFT;
    edit_user_leaf_range(root, vaddr, pages, |e| {
        if e & PTE_PRESENT == 0 {
            e
        } else {
            (e & !(0xFu64 << PTE_PKEY_SHIFT)) | k
        }
    })
}

// ── Copy-on-write fork support ───────────────────────────────────────

/// Copy-on-write clone of the user (low-half) page-table *structure*.
///
/// Rebuilds an identical low-half hierarchy under `child_root`, sharing every
/// leaf frame by reference. Writable leaves must ALREADY be downgraded in the
/// parent (`user_leaf_write_protect_range` before calling); child entries
/// inherit the parent's leaf verbatim, so both sides take a write fault on
/// first touch and `mm::fault` resolves by copy or in-place upgrade.
///
/// `skip` excludes a VA range from the clone entirely — used for the
/// supervisor caps window, which the caller re-establishes privately at its
/// own (per-task randomized) base. Leaves inside the range are neither copied
/// nor refcounted; intermediate tables leading only into the range come over
/// empty and are reclaimed by the usual empty-table sweeps later.
///
/// Every cloned leaf is registered as shared (`framecnt::share_frame`)
/// during the walk — INV-FC-01: this happens strictly before the child root
/// becomes schedulable. Intermediate tables are freshly allocated per child,
/// never tracked, and carry the same flags `map_4k`'s table fan-out uses.
///
/// Huge (PS) leaves anywhere in the low half are rejected: user space is
/// 4 KiB-granular by construction (`commit_pages`), so encountering one means
/// corruption — fail rather than share something we can't refcount.
///
/// On mid-walk OOM everything is unwound: allocated tables are freed and
/// every share taken so far is dropped, leaving parent and allocator exactly
/// as before.
///
/// # Locking
/// Takes [`super::lock`] for the whole walk; caller must not hold it.
pub fn clone_user_space_cow(
    parent_root: u64,
    child_root: u64,
    alloc: &mut BitmapAllocator,
    skip: Option<(u64, u64)>,
) -> Result<(), ()> {
    let _guard = super::lock();
    let mut tables: Vec<u64> = Vec::new();
    let mut shared: Vec<u64> = Vec::new();
    let r = unsafe {
        cow_walk(
            pte_deref(parent_root),
            pte_deref(child_root),
            0,
            0,
            alloc,
            &mut tables,
            &mut shared,
            skip,
        )
    };
    if r.is_err() {
        // Unwind in reverse order of allocation. Table frames were never
        // visible to any walker outside this lock; the shares are dropped
        // with decref_or_free, which can never free here because the parent
        // still holds its reference (every counter was ≥2 when incremented).
        for f in tables.drain(..).rev() {
            unsafe { alloc.free(f) };
        }
        for p in shared.drain(..) {
            // `unshare`, not `decref_or_free`: the parent still holds the
            // frame, and a bare decref would strand it at counter 1 (tracked
            // sole-owner) instead of restoring the pre-fork untracked state.
            crate::mm::framecnt::unshare(p);
        }
    }
    r
}

/// One level of the COW structure copy: `level` 0 = PML4 … 3 = PT.
/// `base_va` carries the already-resolved upper index bits of this subtree.
///
/// # Safety
/// Both table pointers must be valid physmap derefs of present table frames;
/// caller must hold [`super::lock`].
unsafe fn cow_walk(
    ptbl: *mut u64,
    ctbl: *mut u64,
    level: usize,
    base_va: u64,
    alloc: &mut BitmapAllocator,
    tables: &mut Vec<u64>,
    shared: &mut Vec<u64>,
    skip: Option<(u64, u64)>,
) -> Result<(), ()> {
    const SHIFT: [u32; 4] = [39, 30, 21, 12];
    unsafe {
        for i in 0..512usize {
            let va = base_va | ((i as u64) << SHIFT[level]);
            if let Some((lo, hi)) = skip {
                if level == 3 && va >= lo && va < hi {
                    continue;
                }
                if level < 3 && va >= hi {
                    // Subtree starts entirely above the skipped range.
                    continue;
                }
            }
            let e = read_pte(ptbl, i);
            if e & PTE_PRESENT == 0 {
                continue;
            }
            if level < 3 {
                if e & PTE_PS != 0 {
                    return Err(());
                }
                let Some(f) = alloc.alloc() else {
                    return Err(());
                };
                core::ptr::write_bytes(pte_deref(f) as *mut u8, 0, PAGE_SIZE as usize);
                tables.push(f);
                write_pte(ctbl, i, f | PTE_PRESENT | PTE_WRITABLE | PTE_ACCESSED | (e & PTE_USER));
                cow_walk(
                    pte_deref(pte_frame(e)),
                    pte_deref(f),
                    level + 1,
                    va,
                    alloc,
                    tables,
                    shared,
                    skip,
                )?;
            } else {
                // Leaf: inherit verbatim (W already cleared by the caller's
                // sweep) and register the extra owner before returning.
                write_pte(ctbl, i, e);
                let frame = pte_frame(e);
                crate::mm::framecnt::share_frame(frame);
                shared.push(frame);
            }
        }
    }
    Ok(())
}

/// Allocate a fresh, zeroed page-table root and copy the kernel's higher-half
/// mappings (and only those) from `parent_root`, leaving the low half empty.
///
/// The higher half is the canonical negative-address range (VAs with bit 47
/// set — at or above `KERNEL_VMA_BASE`), i.e. PML4 indices `256..=511`.  With
/// `KERNEL_VMA_BASE = 0xFFFFFFFF80000000` the higher half is the very top of
/// canonical space (the PML4 slot 511 region).  The
/// copied PML4 entries reference the parent's shared PDPT subtrees, so those
/// tables must stay alive as long as the clone does.  This gives a new domain
/// its own address space while keeping the kernel image, heap, physmap and
/// device windows reachable.
///
/// # Intentional shared-subtree (do NOT "simplify" to a per-domain rebuild)
/// The ACPI/ECAM/DMA device-window PML4 entries are pre-populated in the
/// kernel root BEFORE this clone (`bootstrap`), and the device sweep maps
/// ECAM/DMA/MMIO lazily into the kernel root AFTER it (`pci::init` passes the
/// kernel root).  Sharing the PML4 entries — and therefore their PDPT/PD/PT
/// subtrees — is exactly what keeps those later kernel-root mappings visible
/// under the clone's CR3.  Rebuilding the windows from layout constants in the
/// clone would leave the driver pointing at empty private subtrees and fault
/// on the first device access.  The parent root outlives every clone (the
/// boot domain is never torn down), so the shared-subtree lifetime is bounded
/// by the kernel itself — there is no per-domain teardown hazard.
///
/// # Panics
/// - If the allocator cannot supply a root frame (OOM).
pub fn clone_high_half(alloc: &mut BitmapAllocator, parent_root: u64) -> u64 {
    // Clone creation must be serialized with map/unmap and the clone fan-out.
    // Without this lock, a concurrent higher-half map can be copied halfway,
    // or a destroy path can unregister/free a root while sync_clone_half is
    // iterating the registry.
    let _guard = super::lock();
    let new_root = alloc.alloc().expect("x86_64 VMM: OOM for cloned root");
    let new_pml4 = pte_deref(new_root);
    let parent_pml4 = pte_deref(parent_root);
    unsafe {
        core::ptr::write_bytes(new_pml4 as *mut u8, 0, 4096);
        // Copy only the top 256 PML4 entries (indices 256..=511) covering the
        // higher half.  Entries 0..255 (the low half, user-space) stay empty.
        for i in 256..=511 {
            write_pte(new_pml4, i, read_pte(parent_pml4, i));
        }
    }
    // Register only after the root is fully initialized, while the same lock
    // excludes map fan-out and teardown.
    super::register_clone_root(new_root);
    new_root
}

/// Ensure the PML4 entry covering `vaddr` is present (allocating a zeroed PDPT
/// frame if needed), so a later `clone_high_half` copies a *present* entry and
/// the two tables share the same higher-half subtree.
///
/// The mapping levels below the PDPT are created on demand by `map_4k`/`map_2m`
/// (the `x86_64` crate's mapper allocates missing intermediate tables), so only
/// the PML4 entry itself must pre-exist for the shared-subtree property to hold.
///
/// # Panics
/// - If the allocator cannot supply a frame (OOM).
pub fn prepopulate_window(alloc: &mut BitmapAllocator, root: u64, vaddr: u64) {
    let i = ((vaddr >> 39) & 0x1FF) as usize;
    // PML4 mutation + clone fan-out under the VMM lock; shootdown after.
    let populated = unsafe {
        let _guard = super::lock();
        let pml4 = pte_deref(root);
        if read_pte(pml4, i) & PTE_PRESENT == 0 {
            let frame = alloc.alloc().expect("prepopulate_window: OOM for PDPT");
            core::ptr::write_bytes(pte_deref(frame) as *mut u8, 0, 4096);
            write_pte(pml4, i, frame | PTE_PRESENT | (1u64 << 1)); // PRESENT | WRITABLE
            // A new higher-half slot: fan it out to live clones too.
            sync_clone_half(root);
            true
        } else {
            false
        }
    };
    if populated {
        super::flush_tlb();
        super::shootdown_tlb();
    }
}

/// Switch to the given root table (physical address of the PML4).
///
/// # Safety
/// The caller must ensure the new page table maps the current instruction
/// stream and stack.
pub unsafe fn activate(root: u64) {
    super::set_current_root(root);
    let frame = PhysFrame::containing_address(XPhysAddr::new(root));
    unsafe {
        Cr3::write(frame, Cr3Flags::empty());
    }
}
