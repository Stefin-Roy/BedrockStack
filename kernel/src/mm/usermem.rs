//! Eager user-process memory management.
//!
//! Owns the per-process address-space bookkeeping (region table) and the
//! commit/release primitives used by the loader and the ring-3
//! `/proc/self:brk|mmap|munmap` unispace methods.
//!
//! # Contract (INV-UM-01..06, see `Invariants/invariants-26-user-mem.md`)
//! - The backing frame(s) for every user mapping are allocated, zeroed and
//!   mapped **synchronously when the mapping is created** (process spawn,
//!   `brk` grow, `mmap`). There is no demand paging, lazy commit, copy-on-write
//!   or swap; #PF is never used to allocate.
//! - No overcommit: a commit that cannot obtain frames fails and rolls back —
//!   nothing is left half-mapped or half-bookkept.
//! - Attachment honours W^X: executable mappings are never writable. The
//!   `mmap` API rejects an RWX request outright.
//!
//! # Teardown
//! The region table is *bookkeeping + validation + introspection*, not the
//! reclaimer: on process exit `mm::vmm::destroy_root` still walks the low half
//! and frees every present leaf frame + intermediate table. `unregister` only
//! drops this table's kernel-heap `Vec`s. The two must stay in step — a region
//! that is *unmapped* at run time (shrunk `brk`, `munmap`) has already released
//! its frames, so `destroy_root` simply finds no PTE for it.

use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use crate::mm::layout::to_physmap;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

fn try_get_alloc() -> Option<*const BitmapAllocator> {
    let ptr = crate::mm::heap::phys_allocator_raw();
    if ptr.is_null() { None } else { Some(ptr as *const BitmapAllocator) }
}

/// 4 KiB page.
pub const PAGE: u64 = 4096;

/// Default per-process committed frame budget (256 MiB), the hard ceiling for
/// `brk` + `mmap` combined. The physical allocator is the final arbiter — this
/// is an additional guard so one runaway process cannot starve every driver.
/// Effective budget is dynamic: min(256MiB, free_ram/16) in pages, where free_ram
/// is current free frames * 4K. This throttles when RAM is scarce.
const USER_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
const USER_BUDGET_CEILING_PAGES: u64 = USER_BUDGET_BYTES / PAGE;
/// Minimum dynamic budget floor: 16 MiB so tiny-free systems can still make progress.
const USER_BUDGET_FLOOR_PAGES: u64 = (16 * 1024 * 1024) / PAGE;

/// Compute dynamic budget based on free RAM left. Keeps ceiling but throttles
/// when free is low. Called with the allocator on every brk/mmap/register
/// budget check.
///
/// The frame count comes from the allocator's O(1) atomic counter, so callers
/// read it *under* the `ADDR_SPACES` lock — making the budget check atomic
/// with respect to every other process's check and closing the old
/// compute-before-lock TOCTOU where two processes could both pass and then
/// exhaust physical RAM together (`commit_pages` still rolls back atomically,
/// so the residual failure mode is a clean ENOMEM).
fn dynamic_budget_pages(alloc: &BitmapAllocator) -> u64 {
    let free_pages = alloc.free_frames() as u64;
    let frac = free_pages / 16;
    let clamped = frac.clamp(USER_BUDGET_FLOOR_PAGES, USER_BUDGET_CEILING_PAGES);
    // Also clamp by free/4 to not promise more than we could actually give
    let by_free_quarter = free_pages / 4;
    core::cmp::min(clamped, by_free_quarter.max(USER_BUDGET_FLOOR_PAGES))
}

#[allow(dead_code)]
#[inline]
fn effective_budget_pages(_asp: &AddressSpace, alloc: &BitmapAllocator) -> u64 {
    // Genuinely dynamic: free/16 capped, not `min(snapshot, dyn)` which would
    // pin a process to a low budget forever if forked under pressure. `asp.budget_pages`
    // remains for introspection (`/proc/.../mem`) but is not a hard cap for growth —
    // throttling is global, ceiling is `USER_BUDGET_CEILING_PAGES`.
    dynamic_budget_pages(alloc)
}

/// errno codes returned from the syscall-facing functions (already negated).
const EFAULT: i64 = -14;
const ENOMEM: i64 = -12;
const EINVAL: i64 = -22;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RegionKind {
    /// ELF PT_LOAD image footprint (spawned once, never unmapable).
    Image = 0,
    /// The user stack (guard page below).
    Stack = 1,
    /// The `brk` break region [brk_floor, brk_cur).
    Heap = 2,
    /// An anonymous `mmap` region (guard page below).
    Anon = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub kind: RegionKind,
    pub vaddr: u64,
    pub pages: u64,
    /// Informational permissions (introspection only; the PTE is the truth).
    /// For lazy regions this is also the fill permission used by
    /// `demand_fill`.
    pub flags: PageFlags,
    /// True when ≥1 unmapped page must protect this region's underside.
    pub guard: bool,
    /// Lazy commit (mmap prot bit 0x8): registered without frames; pages
    /// materialize on first touch via `demand_fill` at `flags`. Charged per
    /// fault instead of up front (UM-010).
    pub lazy: bool,
    /// PKU protection key 0..=15 (0 = default, unrestricted). Applied to
    /// present leaves by `pkey_protect`; lazy/COW fills re-apply it from
    /// here when they materialize a page. Enforcement is PKRU-side.
    pub pkey: u8,
}

impl Region {
    fn end(&self) -> u64 {
        self.vaddr + self.pages * PAGE
    }
}

/// One live process address space, keyed by an index stored in its `Task`.
pub struct AddressSpace {
    pub root: u64,
    /// Page-aligned end of the ELF image — the initial program break.
    pub brk_floor: u64,
    /// Current committed program break (page-aligned).
    pub brk_cur: u64,
    pub stack_top: u64,
    /// Number of committed (frame-backed) pages across all regions.
    pub committed: u64,
    pub budget_pages: u64,
    pub regions: Vec<Region>,
}

struct Table {
    slots: Vec<Option<AddressSpace>>,
    free: Vec<usize>,
}

static ADDR_SPACES: Mutex<Table> = Mutex::new(Table {
    slots: Vec::new(),
    free: Vec::new(),
});

// ── Registry ─────────────────────────────────────────────────────────

/// Allocate a slot and register a fresh address space.
///
/// `image_floor`/`image_top` are the page-aligned extent of the ELF PT_LOAD
/// footprint, `stack_top` the 32 KiB user stack ceiling (its 8 pages live
/// immediately below), and `stack_flags` the stack's page permissions.
///
/// Committed-page accounting starts at image + stack pages.
/// Budget is dynamic (free/16 capped to 256MiB) but register initializes to
/// ceiling; effective budget is checked per-brk/mmap via `effective_budget_pages`.
pub fn register(
    root: u64,
    image_floor: u64,
    image_top: u64,
    stack_top: u64,
    stack_flags: PageFlags,
) -> usize {
    let mut table = ADDR_SPACES.lock();
    let stack_base = stack_top - 8 * PAGE;
    let committed = (image_top - image_floor) / PAGE + 8;
    // Per-boot fraction via free ram if allocator live: snapshot dynamic budget at register time
    let init_budget = if let Some(alloc_ptr) = try_get_alloc() {
        let alloc = unsafe { &*alloc_ptr };
        dynamic_budget_pages(alloc).max(committed + 16)
    } else {
        USER_BUDGET_CEILING_PAGES
    };
    let asp = AddressSpace {
        root,
        brk_floor: image_top,
        brk_cur: image_top,
        stack_top,
        committed,
        budget_pages: init_budget.min(USER_BUDGET_CEILING_PAGES),
        regions: vec![
            Region {
                kind: RegionKind::Image,
                vaddr: image_floor,
                pages: (image_top - image_floor) / PAGE,
                flags: PageFlags::USER | PageFlags::READ | PageFlags::WRITE,
                guard: false,
                lazy: false,
                pkey: 0,
            },
            Region {
                kind: RegionKind::Stack,
                vaddr: stack_base,
                pages: 8,
                flags: stack_flags,
                guard: true,
                lazy: false,
                pkey: 0,
            },
        ],
    };
    match table.free.pop() {
        Some(i) => {
            table.slots[i] = Some(asp);
            i
        }
        None => {
            table.slots.push(Some(asp));
            table.slots.len() - 1
        }
    }
}

/// Release a slot after the process root has been destroyed. Frames are NOT
/// freed here — `destroy_root` already walked the page tables and returned every
/// leaf. This only drops the region table's kernel-heap `Vec`s.
pub fn unregister(idx: usize) {
    let mut table = ADDR_SPACES.lock();
    if idx < table.slots.len() && table.slots[idx].is_some() {
        table.slots[idx] = None;
        table.free.push(idx);
    }
}

/// Read-only snapshot for `/proc/<pid>/mem` introspection.
pub struct MemSummary {
    pub root: u64,
    pub brk_floor: u64,
    pub brk_cur: u64,
    pub stack_top: u64,
    pub committed_pages: u64,
    pub budget_pages: u64,
    pub regions: Vec<(u8, u64, u64)>,
}

pub fn summarize(idx: usize) -> Option<MemSummary> {
    let table = ADDR_SPACES.lock();
    let asp = table.slots.get(idx)?.as_ref()?;
    Some(MemSummary {
        root: asp.root,
        brk_floor: asp.brk_floor,
        brk_cur: asp.brk_cur,
        stack_top: asp.stack_top,
        committed_pages: asp.committed,
        budget_pages: asp.budget_pages,
        regions: asp
            .regions
            .iter()
            .map(|r| (r.kind as u8, r.vaddr, r.pages))
            .collect(),
    })
}

// ── System-call entry points ─────────────────────────────────────────
//
// All functions validate first, then commit frames, then touch bookkeeping —
// an OOM or invalid argument leaves the address space unchanged.

/// `brk(new_break)`: grow or shrink the committed program break.
///
/// `new_break == 0` (or below the floor) is a query returning the current
/// break. Growth page-rounds up and eagerly commits zeroed, read/write, NX
/// frames; the region table is only updated after the commit succeeds.
pub fn brk(idx: usize, new_break: u64, alloc: &mut BitmapAllocator) -> Result<u64, i64> {
    let mut table = ADDR_SPACES.lock();
    let Some(asp) = table.slots.get_mut(idx).and_then(|s| s.as_mut()) else {
        return Err(EFAULT);
    };

    if new_break == 0 {
        return Ok(asp.brk_cur);
    }
    let Some(new) = align_up(new_break) else {
        return Err(EINVAL);
    };
    if new < asp.brk_floor {
        return Ok(asp.brk_cur);
    }
    let old = asp.brk_cur;
    if new == old {
        return Ok(old);
    }

    if new > old {
        // Growth boundary: the lowest occupied span start above the current
        // break. Anon/stack regions have guard pages embedded in their span,
        // so staying at-or-below it keeps a clean separation.
        let mut boundary = u64::MAX;
        for r in &asp.regions {
            let (start, _end) = coll_span(r);
            if start > old {
                boundary = boundary.min(start);
            }
        }
        if new > boundary {
            return Err(ENOMEM);
        }
        // Budget read under ADDR_SPACES (O(1) counter) — atomic vs other checks.
        let eff_pages = dynamic_budget_pages(&*alloc);
        let npages = (new - old) / PAGE;
        if npages > eff_pages.saturating_sub(asp.committed) {
            return Err(ENOMEM);
        }
        let mut vmm = Vmm::from_root(asp.root);
        let flags = PageFlags::USER | PageFlags::READ | PageFlags::WRITE;
        if commit_pages(&mut vmm, alloc, old, npages, flags).is_err() {
            return Err(ENOMEM);
        }
        asp.brk_cur = new;
        asp.committed += npages;
        match asp.regions.iter_mut().find(|r| r.kind == RegionKind::Heap) {
            Some(h) => h.pages = (new - asp.brk_floor) / PAGE,
            None => asp.regions.push(Region {
                kind: RegionKind::Heap,
                vaddr: asp.brk_floor,
                pages: (new - asp.brk_floor) / PAGE,
                flags,
                guard: false,
                lazy: false,
                pkey: 0,
            }),
        }
    } else {
        // Shrink: update bookkeeping under lock, then release frames after dropping it
        // so the cross-CPU TLB shootdown (inside release_pages) does not run while
        // holding ADDR_SPACES – another CPU spinning on that lock would still be
        // able to take the IPI because the lock is spin::Mutex (IRQs enabled).
        let npages = (old - new) / PAGE;
        let root = asp.root;
        let heap_pages = (new - asp.brk_floor) / PAGE;
        asp.brk_cur = new;
        debug_assert!(npages <= asp.committed, "brk shrink {} > committed {}", npages, asp.committed);
        asp.committed -= npages;
        if heap_pages == 0 {
            asp.regions.retain(|r| r.kind != RegionKind::Heap);
        } else if let Some(h) = asp.regions.iter_mut().find(|r| r.kind == RegionKind::Heap) {
            h.pages = heap_pages;
        }
        drop(table);
        let mut vmm = Vmm::from_root(root);
        release_pages(&mut vmm, alloc, new, npages);
        return Ok(new);
    }
    Ok(asp.brk_cur)
}

/// `mmap(addr, len, prot)`: commit `len` anonymous pages.
///
/// `addr == 0` picks the first available gap above the break (page-aligned,
/// guard page between regions, capped by the user stack's guard). A fixed
/// `addr` must be page-aligned and free. `len` must be page-aligned and > 0.
///
/// `prot` bits: 1=read, 2=write, 4=exec (W^X: exec+write rejected), and
/// **8 = lazy** (UM-010): the region registers with no backing frames and no
/// committed charge; each page materializes zeroed on first touch through
/// `demand_fill`, charged against the dynamic budget then. Lazy is only
/// meaningful for writable mappings — a read-only lazy region would fault-fill
/// nothing, so it is rejected.
pub fn mmap(
    idx: usize,
    addr: u64,
    len: u64,
    prot: u64,
    alloc: &mut BitmapAllocator,
) -> Result<u64, i64> {
    let lazy = prot & 0x8 != 0;
    let prot = prot & !0x8;
    let mut table = ADDR_SPACES.lock();
    let Some(asp) = table.slots.get_mut(idx).and_then(|s| s.as_mut()) else {
        return Err(EFAULT);
    };
    if len == 0 || len % PAGE != 0 {
        return Err(EINVAL);
    }
    let npages = len / PAGE;
    let flags = prot_to_flags(prot)?;
    if lazy && !flags.contains(PageFlags::WRITE) {
        return Err(EINVAL);
    }

    let vaddr = if addr == 0 {
        match find_fit(asp, npages) {
            Some(v) => v,
            None => return Err(ENOMEM),
        }
    } else {
        if addr % PAGE != 0 || addr < PAGE {
            return Err(EINVAL);
        }
        match addr.checked_add(len) {
            Some(end) if end <= user_ceiling(asp) => {}
            _ => return Err(EINVAL),
        }
        if collides(asp, addr, len) {
            return Err(EINVAL);
        }
        addr
    };

    if !lazy {
        // Budget read under ADDR_SPACES (O(1) counter) — atomic vs other checks.
        let eff_pages = dynamic_budget_pages(&*alloc);
        if npages > eff_pages.saturating_sub(asp.committed) {
            return Err(ENOMEM);
        }
        let mut vmm = Vmm::from_root(asp.root);
        if commit_pages(&mut vmm, alloc, vaddr, npages, flags).is_err() {
            return Err(ENOMEM);
        }
        asp.committed += npages;
    }
    asp.regions.push(Region {
        kind: RegionKind::Anon,
        vaddr,
        pages: npages,
        flags,
        guard: true,
        lazy,
        pkey: 0,
    });
    Ok(vaddr)
}

/// `munmap(addr, len)`: release and unmap whole or partial anonymous regions.
///
/// Accepts partial-region unmaps for `Anon` via split: a subrange inside an
/// `Anon` punches a hole, splitting the region into head/tail remainders.
/// Whole-region runs are still supported. Attempts on Image/Stack/Heap are
/// rejected with `EINVAL`.
pub fn munmap(idx: usize, addr: u64, len: u64, alloc: &mut BitmapAllocator) -> Result<(), i64> {
    if len == 0 || len % PAGE != 0 || addr % PAGE != 0 {
        return Err(EINVAL);
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return Err(EINVAL),
    };
    if end <= addr {
        return Err(EINVAL);
    }
    // Phase 1: validate and patch region table under lock, collecting unmap work for phase 2.
    let (root, to_unmap) = {
        let mut table = ADDR_SPACES.lock();
        let Some(asp) = table.slots.get_mut(idx).and_then(|s| s.as_mut()) else {
            return Err(EFAULT);
        };
        // Collect overlapping Anon regions. Reject non-Anon overlap.
        let mut hits: Vec<usize> = Vec::new();
        for (i, r) in asp.regions.iter().enumerate() {
            if r.end() <= addr || r.vaddr >= end {
                continue;
            }
            if r.kind != RegionKind::Anon {
                return Err(EINVAL);
            }
            hits.push(i);
        }
        if hits.is_empty() {
            return Err(EINVAL);
        }
        hits.sort_unstable_by_key(|&i| asp.regions[i].vaddr);
        let mut cursor = addr;
        for &i in &hits {
            let r = asp.regions[i];
            let ov_start = r.vaddr.max(addr);
            let ov_end = r.end().min(end);
            if ov_start > cursor {
                return Err(EINVAL);
            }
            if ov_start == cursor {
                cursor = ov_end;
            } else {
                return Err(EINVAL);
            }
            if cursor >= end { break; }
        }
        if cursor < end {
            return Err(EINVAL);
        }
        hits.sort_unstable_by(|a, b| b.cmp(a));
        let root = asp.root;
        let mut to_unmap: Vec<(u64, u64)> = Vec::new();
        for &idx_hit in &hits {
            let r = asp.regions[idx_hit];
            let ov_start = r.vaddr.max(addr);
            let ov_end = r.end().min(end);
            let ov_pages = (ov_end - ov_start) / PAGE;
            to_unmap.push((ov_start, ov_pages));
            if ov_start == r.vaddr && ov_end == r.end() {
                asp.regions.remove(idx_hit);
            } else if ov_start == r.vaddr {
                let tail_pages = (r.end() - ov_end) / PAGE;
                asp.regions[idx_hit].vaddr = ov_end;
                asp.regions[idx_hit].pages = tail_pages;
            } else if ov_end == r.end() {
                let head_pages = (ov_start - r.vaddr) / PAGE;
                asp.regions[idx_hit].pages = head_pages;
            } else {
                let head_pages = (ov_start - r.vaddr) / PAGE;
                let tail_pages = (r.end() - ov_end) / PAGE;
                let gap_pages = (ov_end - ov_start) / PAGE;
                let tail_guard = gap_pages >= 1;
                let tail = Region {
                    kind: RegionKind::Anon,
                    vaddr: ov_end,
                    pages: tail_pages,
                    flags: r.flags,
                    guard: tail_guard,
                    lazy: r.lazy,
                    pkey: r.pkey,
                };
                asp.regions[idx_hit].pages = head_pages;
                asp.regions.insert(idx_hit + 1, tail);
            }
        }
        // Committed accounting is deferred to phase 3: lazy regions register
        // without frames, so only the leaves `release_pages` actually collects
        // may be subtracted (an eager region yields exactly its span; a lazy
        // one yields only the pages that ever fault-filled). Subtracting the
        // raw span up front would underflow `committed` on a partial unmap of
        // a sparsely-filled lazy region.
        (root, to_unmap)
    };
    // Phase 2: outside lock – shootdown + free
    let mut vmm = Vmm::from_root(root);
    let mut actually_freed = 0u64;
    for (vaddr, npages) in to_unmap {
        let mut frames: Vec<u64> = Vec::new();
        vmm.unmap_range_collect(alloc, vaddr, npages * PAGE, &mut frames);
        for phys in frames {
            crate::mm::framecnt::decref_or_free(alloc, phys);
            actually_freed += 1;
        }
    }
    // Phase 3: reconcile committed with what was really released.
    {
        let mut table = ADDR_SPACES.lock();
        if let Some(asp) = table.slots.get_mut(idx).and_then(|s| s.as_mut()) {
            debug_assert!(
                actually_freed <= asp.committed,
                "munmap freed {} > committed {}",
                actually_freed,
                asp.committed
            );
            asp.committed -= actually_freed.min(asp.committed);
        }
    }
    Ok(())
}

/// `fork`: clone `parent_vm` copy-on-write. Returns `(child slot, child root)`.
///
/// `skip_cow` excludes a VA range (the parent's supervisor caps window) from
/// the structural clone — the caller re-establishes it privately at its own
/// randomized base.
///
/// Both address spaces keep identical region bookkeeping (brk, stack top,
/// committed accounting); physical frames become shared (`framecnt` > 1) and
/// every writable leaf is downgraded to read-only in BOTH roots before this
/// returns. The first write to any shared page faults into the CoW resolver,
/// which copies or upgrades based on the refcount — so the parent's stale-W
/// window between the PTE sweep and the shootdown below is unexploitable:
/// the BSP-only scheduler guarantees neither task can run in it.
///
/// On failure the child's tables/shares are unwound by
/// [`crate::mm::vmm::clone_user_space_cow`] and nothing is registered.
#[cfg(target_arch = "x86_64")]
pub fn fork_as(
    parent_vm: usize,
    alloc: &mut BitmapAllocator,
    skip_cow: Option<(u64, u64)>,
) -> Result<(usize, u64), i64> {
    let mut table = ADDR_SPACES.lock();
    let Some(parent) = table.slots.get(parent_vm).and_then(|s| s.as_ref()) else {
        return Err(EFAULT);
    };

    // Fresh root: higher half shared by reference (kernel windows), low half
    // filled by the structural COW clone below.
    let child_root = crate::mm::vmm::clone_high_half(alloc, parent.root);

    // 1) Downgrade writable leaves in the parent. Skipped pages are simply
    //    absent leaves; the child inherits them as absent too.
    for r in &parent.regions {
        if r.flags.contains(PageFlags::WRITE) && r.pages > 0 {
            crate::mm::vmm::user_leaf_write_protect_range(parent.root, r.vaddr, r.pages);
        }
    }

    // 2) Structural low-half clone sharing every leaf (INV-FC-01).
    if crate::mm::vmm::clone_user_space_cow(parent.root, child_root, alloc, skip_cow).is_err() {
        // Roll the parent back to fully writable before unwinding the child:
        // the downgrade sweep must never outlive the fork attempt (atomicity
        // — the caller sees either a working fork or an untouched parent).
        for r in &parent.regions {
            if r.flags.contains(PageFlags::WRITE) && r.pages > 0 {
                crate::mm::vmm::user_leaf_make_writable_range(parent.root, r.vaddr, r.pages);
            }
        }
        drop(table);
        crate::mm::vmm::destroy_root(child_root, alloc);
        return Err(ENOMEM);
    }

    // 3) Child bookkeeping: verbatim snapshot of the parent's metadata.
    let child = AddressSpace {
        root: child_root,
        brk_floor: parent.brk_floor,
        brk_cur: parent.brk_cur,
        stack_top: parent.stack_top,
        committed: parent.committed,
        budget_pages: parent.budget_pages,
        regions: parent.regions.clone(),
    };
    let idx = match table.free.pop() {
        Some(i) => {
            table.slots[i] = Some(child);
            i
        }
        None => {
            table.slots.push(Some(child));
            table.slots.len() - 1
        }
    };
    drop(table);

    // 4) Parent CPUs must observe the downgraded PTEs. The child root has
    //    never been active anywhere; this CPU flushed locally inside the
    //    write-protect sweep, so only the cross-CPU broadcast remains.
    crate::mm::vmm::shootdown_tlb();

    Ok((idx, child_root))
}

// ── Commit / release primitives ──────────────────────────────────────

/// Demand-fill the absent page at `va` from its containing region's flags.
///
/// Only `Heap`/`Stack`/`Anon` regions fill: their pages are legitimately
/// materializable on first touch (and mandatory for lazy commits). An absent
/// page in an `Image` region is a hole the loader never mapped — fatal, as
/// before. The committed counter is charged under the dynamic budget while
/// holding `ADDR_SPACES`, exactly like `brk` growth.
///
/// Returns `false` (→ task killed) when: no region covers `va`, the region
/// is not writable, the budget or allocator is exhausted, or `vm` is stale.
pub fn demand_fill(vm: usize, va: u64) -> bool {
    let alloc = crate::mm::heap::get_phys_allocator_mut();
    let mut table = ADDR_SPACES.lock();
    let Some(asp) = table.slots.get_mut(vm).and_then(|s| s.as_mut()) else {
        return false;
    };
    let Some(r) = asp.regions.iter().find(|r| {
        va >= r.vaddr && va < r.vaddr + r.pages * PAGE
    }) else {
        return false;
    };
    match r.kind {
        RegionKind::Image => return false,
        RegionKind::Heap | RegionKind::Stack | RegionKind::Anon => {}
    }
    if !r.flags.contains(PageFlags::WRITE) {
        // Read-only regions are eagerly mapped; absence is corruption.
        return false;
    }
    if asp.committed + 1 > dynamic_budget_pages(&*alloc) {
        return false; // over budget → OOM-kill policy
    }
    let Some(phys) = alloc.alloc() else {
        return false; // physical exhaustion → OOM-kill policy
    };
    unsafe {
        core::ptr::write_bytes(crate::mm::layout::to_physmap(phys) as *mut u8, 0, PAGE as usize);
    }
    let mut vmm = Vmm::from_root(asp.root);
    vmm.map_4k(alloc, va, phys, r.flags);
    #[cfg(target_arch = "x86_64")]
    if r.pkey != 0 {
        crate::mm::vmm::user_leaf_set_pkey_range(asp.root, va, 1, r.pkey);
    }
    asp.committed += 1;
    true
}

/// Snapshot `(root, region-is-writable)` for the CoW resolver. `None` when
/// `vm` is stale or no region covers `va` (guard pages included — they are
/// outside every span by construction).
pub fn cow_context(vm: usize, va: u64) -> Option<(u64, bool)> {
    let table = ADDR_SPACES.lock();
    let asp = table.slots.get(vm)?.as_ref()?;
    let r = asp
        .regions
        .iter()
        .find(|r| va >= r.vaddr && va < r.vaddr + r.pages * PAGE)?;
    Some((asp.root, r.flags.contains(PageFlags::WRITE)))
}

/// `pkey_mprotect(addr, len, key)`: tag a whole `Anon`/`Heap` region with a
/// PKU protection key (0 clears). The span must match the region exactly —
/// sub-range tagging would need region splitting for metadata consistency.
///
/// Present leaves are re-keyed in one batched edit (shootdown included);
/// absent (lazy / never-COW-copied) pages pick the key up from region
/// metadata when they materialize. Enforcement happens through PKRU rights
/// (`/proc/self:pkey_set`, `Task.pkru`); tagging alone changes nothing until
/// the task restricts itself.
#[cfg(target_arch = "x86_64")]
pub fn pkey_protect(vm: usize, addr: u64, len: u64, key: u8) -> Result<(), i64> {
    if len == 0 || len % PAGE != 0 || addr % PAGE != 0 {
        return Err(EINVAL);
    }
    let mut table = ADDR_SPACES.lock();
    let Some(asp) = table.slots.get_mut(vm).and_then(|s| s.as_mut()) else {
        return Err(EFAULT);
    };
    let Some(r) = asp.regions.iter_mut().find(|r| {
        r.vaddr == addr && r.pages * PAGE == len
    }) else {
        return Err(EINVAL);
    };
    if !matches!(r.kind, RegionKind::Anon | RegionKind::Heap) {
        return Err(EINVAL);
    }
    r.pkey = key;
    crate::mm::vmm::user_leaf_set_pkey_range(asp.root, addr, len / PAGE, key);
    drop(table);
    crate::mm::vmm::shootdown_tlb();
    Ok(())
}

/// Eagerly allocate, zero and map `npages` at `vaddr`. Non-contiguous frames
/// are fine — mappings are 4 KiB granular throughout.
///
/// On the first allocation failure everything already mapped in this call is
/// unmapped and freed (the intermediate tables `map_4k` built collapse too),
/// so the address space is left exactly as it was: no partial commit.
fn commit_pages(
    vmm: &mut Vmm,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    npages: u64,
    flags: PageFlags,
) -> Result<(), ()> {
    let mut va = vaddr;
    // Collect phys frames to free on OOM; use dedicated vec to avoid translate-after-unmap races
    let mut phys_vec: Vec<u64> = Vec::new();
    let mut mapped: Vec<u64> = Vec::new();
    for _ in 0..npages {
        let Some(phys) = alloc.alloc() else {
            // Rollback already mapped pages in one batched unmap+single shootdown
            if !mapped.is_empty() {
                // mapped holds vaddrs; collect their phys from phys_vec
                let mut frames: Vec<u64> = Vec::new();
                let base = mapped[0];
                let size = (mapped.len() as u64) * PAGE;
                // Use the phys we stashed instead of translate to avoid double-free confusion
                vmm.unmap_range_collect(alloc, base, size, &mut frames);
                // Free using stashed phys count, but frames already contains them;
                // prefer frames from VMM to keep accounting consistent.
                // Refcount-aware: shared (CoW) frames only drop one reference.
                for p in frames {
                    crate::mm::framecnt::decref_or_free(alloc, p);
                }
                // Any remaining phys in phys_vec beyond mapped len already not mapped
                // (alloc succeeded but map failed? not here) — none.
            }
            // Free any phys that were allocated but not yet mapped (none in this path, but keep for symmetry)
            for p in phys_vec.iter().skip(mapped.len()) {
                crate::mm::framecnt::decref_or_free(alloc, *p);
            }
            return Err(());
        };
        unsafe {
            core::ptr::write_bytes(to_physmap(phys) as *mut u8, 0, PAGE as usize);
        }
        vmm.map_4k(alloc, va, phys, flags);
        phys_vec.push(phys);
        mapped.push(va);
        va += PAGE;
    }
    Ok(())
}

/// Unmap and free every frame backing `[vaddr, vaddr + npages*PAGE)`.
///
/// All PTEs in the range are cleared first, then a single batched TLB
/// shootdown is broadcast before any frame (leaf or intermediate table) is
/// released — required here because the process root is the *active* CR3
/// during a syscall.  The range-unmap avoids firing one full cross-CPU
/// shootdown per page (the `unmap_4k` loop this replaces).
fn release_pages(vmm: &mut Vmm, alloc: &mut BitmapAllocator, vaddr: u64, npages: u64) {
    let mut frames: Vec<u64> = Vec::new();
    vmm.unmap_range_collect(alloc, vaddr, npages * PAGE, &mut frames);
    for phys in frames {
        // Refcount-aware: a frame still shared with a fork sibling only
        // drops one reference here; untracked frames free exactly as before.
        crate::mm::framecnt::decref_or_free(alloc, phys);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

const fn align_up(x: u64) -> Option<u64> {
    let y = match x.checked_add(PAGE - 1) {
        Some(v) => v,
        None => return None,
    };
    Some(y & !(PAGE - 1))
}

/// The collision span of a region: its pages plus its guard page, if any. The
/// guard protects the region's underside, so it counts as occupied space.
fn coll_span(r: &Region) -> (u64, u64) {
    let guard = if r.guard { PAGE } else { 0 };
    (r.vaddr - guard, r.vaddr + r.pages * PAGE)
}

fn range_intersects(a: u64, alen: u64, bstart: u64, bend: u64) -> bool {
    a < bend && a.checked_add(alen).map(|ae| ae > bstart).unwrap_or(true)
}

/// Highest address user allocations may reach: the stack's guard-page bottom.
fn user_ceiling(asp: &AddressSpace) -> u64 {
    for r in &asp.regions {
        if r.kind == RegionKind::Stack {
            return coll_span(r).0;
        }
    }
    0
}

/// Candidate window for the randomized first-fit pass (64 MiB of pages).
const FIT_ASLR_SPAN_PAGES: u64 = 1 << 14;

/// Random page-aligned probe point between the break and the ceiling, or
/// `None` when the window is too small to bother.
fn random_fit_start(asp: &AddressSpace, len: u64) -> Option<u64> {
    let ceiling = user_ceiling(asp);
    let lo = asp.brk_cur.checked_add(PAGE)?;
    if ceiling <= lo.saturating_add(len) {
        return None;
    }
    let span_pages = ((ceiling - lo - len) / PAGE).min(FIT_ASLR_SPAN_PAGES);
    if span_pages == 0 {
        return None;
    }
    let off = (crate::random::random_u64() % span_pages) * PAGE;
    Some(lo + off)
}

/// First-fit gap search starting at `start` (one guard page above it).
fn find_fit_from(asp: &AddressSpace, npages: u64, start: u64) -> Option<u64> {
    let len = npages * PAGE;
    let ceiling = user_ceiling(asp);
    let mut cand = start;

    loop {
        if cand >= ceiling {
            return None;
        }
        if len > ceiling - cand {
            return None;
        }
        let mut blocked = None;
        for r in &asp.regions {
            let (start, end) = coll_span(r);
            if range_intersects(cand, len, start, end) {
                blocked = Some(end);
                break;
            }
        }
        match blocked {
            Some(end) => {
                let next = end.checked_add(PAGE)?;
                cand = (next).max(cand.checked_add(PAGE)?);
            }
            None => return Some(cand),
        }
    }
}

/// mmap placement: randomized-start first fit with a deterministic fallback.
///
/// The first pass probes from a random offset above the break so consecutive
/// allocations don't line up predictably; if that pass walks off the ceiling,
/// the classic bottom-up scan from `brk_cur + PAGE` still finds any low gap,
/// preserving the old packing guarantees as the worst case.
fn find_fit(asp: &AddressSpace, npages: u64) -> Option<u64> {
    let len_bytes = npages.checked_mul(PAGE)?;
    if let Some(start) = random_fit_start(asp, len_bytes) {
        if let Some(v) = find_fit_from(asp, npages, start) {
            return Some(v);
        }
    }
    find_fit_from(asp, npages, asp.brk_cur.checked_add(PAGE)?)
}

/// True when `[addr, addr+len)` (plus its own guard page below) overlaps any
/// existing region. `addr` must be verified >= PAGE by the caller.
fn collides(asp: &AddressSpace, addr: u64, len: u64) -> bool {
    let start = addr - PAGE;
    let end = addr + len;
    for r in &asp.regions {
        let (rs, re) = coll_span(r);
        if start < re && end > rs {
            return true;
        }
    }
    false
}

/// Translate a raw `prot` bitmask into `PageFlags`, enforcing W^X.
fn prot_to_flags(prot: u64) -> Result<PageFlags, i64> {
    if prot == 0 || prot & !0x7 != 0 {
        return Err(EINVAL);
    }
    let exec = prot & 4 != 0;
    let write = prot & 2 != 0;
    let read = prot & 1 != 0;
    if exec && write {
        return Err(EINVAL); // W^X: an executable mapping is never writable
    }
    let mut f = PageFlags::USER;
    if read {
        f |= PageFlags::READ;
    }
    if exec {
        f |= PageFlags::EXECUTE;
    } else if write {
        f |= PageFlags::WRITE;
    }
    // prot must have at least one of read/write/exec, and we already rejected 0
    Ok(f)
}
