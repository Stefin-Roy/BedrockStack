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

use crate::mm::phys_alloc::BitmapAllocator;
use super::PageFlags;

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
        f |= PageTableFlags::NO_CACHE;         // PCD
        f |= PageTableFlags::WRITE_THROUGH;    // PWT
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
    unsafe { msr.write(new_val); }
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

pub fn map_4k(
    root: u64,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    paddr: u64,
    flags: PageFlags,
) {
    let _guard = super::lock();
    // A brand-new higher-half PML4 slot must be fanned out to every clone.
    let new_slot = unsafe { new_higher_half_slot(root, vaddr) };
    let mut mapper = mapper_at(root);
    let mut frame_alloc = BitmapFrameAllocator { inner: alloc };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::<Size4KiB>::containing_address(XPhysAddr::new(paddr));
    let x86_flags = page_flags_to_x86(flags);

    unsafe {
        mapper
            .map_to_with_table_flags(page, frame, x86_flags, table_flags_for(x86_flags), &mut frame_alloc)
            .expect("x86_64 4KiB map failed")
            .flush();
    }
    if new_slot {
        sync_clone_half(root);
        super::flush_tlb();
        super::shootdown_tlb();
    }
}

pub fn map_2m(
    root: u64,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    paddr: u64,
    flags: PageFlags,
) {
    let _guard = super::lock();
    let new_slot = unsafe { new_higher_half_slot(root, vaddr) };
    let mut mapper = mapper_at(root);
    let mut frame_alloc = BitmapFrameAllocator { inner: alloc };

    let page = Page::<Size2MiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::<Size2MiB>::containing_address(XPhysAddr::new(paddr));
    let x86_flags = page_flags_to_x86(flags);

    unsafe {
        mapper
            .map_to_with_table_flags(page, frame, x86_flags, table_flags_for(x86_flags), &mut frame_alloc)
            .expect("x86_64 2MiB map failed")
            .flush();
    }
    if new_slot {
        sync_clone_half(root);
        super::flush_tlb();
        super::shootdown_tlb();
    }
}

pub fn unmap_4k(root: u64, vaddr: u64, pending: &mut super::PendingFrames) -> bool {
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
    if !removed {
        return false;
    }
    reclaim_empty_tables(root, pending, vaddr);
    true
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
    unsafe { *table.add(index & 0x1FF) = value; }
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
fn reclaim_empty_tables(root: u64, pending: &mut super::PendingFrames, vaddr: u64) {
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
    mapper.translate_addr(VirtAddr::new(vaddr)).map(|p| p.as_u64())
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
        return Some((pte_frame(e2) | (vaddr & 0x1F_FFFF), user_ok, e2 & WRITABLE != 0));
    }

    let pt = pte_deref(pte_frame(e2));
    let e3 = unsafe { read_pte(pt, i3) };
    if e3 & PRESENT == 0 {
        return None;
    }
    Some((pte_frame(e3) | (vaddr & 0xFFF), user_ok & (e3 & USER != 0), e3 & WRITABLE != 0))
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
    for f in frames {
        unsafe { alloc.free(f); }
    }
    super::unregister_clone_root(root);
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
    let mut mapper = mapper_at(root);
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    // Reconstruct flags matching what leaf_flags() would have set for
    // .data/.bss (READ | WRITE | NO_EXECUTE) minus WRITABLE.
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::ACCESSED
        | PageTableFlags::NO_EXECUTE;
    unsafe {
        mapper
            .update_flags(page, flags)
            .expect("make_read_only: update_flags failed")
            .flush();
    }
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
    // Register the clone so `reclaim_empty_tables` stops freeing intermediate
    // tables (the clone shares the parent's higher-half subtrees) and so
    // `sync_clone_half` keeps it sharing any slot populated later.  Do this
    // AFTER the copy so the registry never holds a half-built root.
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
    unsafe {
        let pml4 = pte_deref(root);
        if read_pte(pml4, i) & PTE_PRESENT == 0 {
            let frame = alloc.alloc().expect("prepopulate_window: OOM for PDPT");
            core::ptr::write_bytes(pte_deref(frame) as *mut u8, 0, 4096);
            write_pte(pml4, i, frame | PTE_PRESENT | (1u64 << 1)); // PRESENT | WRITABLE
            // A new higher-half slot: fan it out to live clones too.
            sync_clone_half(root);
            super::flush_tlb();
            super::shootdown_tlb();
        }
    }
}

/// Switch to the given root table (physical address of the PML4).
///
/// # Safety
/// The caller must ensure the new page table maps the current instruction
/// stream and stack.
pub unsafe fn activate(root: u64) {
    let frame = PhysFrame::containing_address(XPhysAddr::new(root));
    unsafe { Cr3::write(frame, Cr3Flags::empty()); }
}
