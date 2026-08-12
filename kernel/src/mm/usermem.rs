//! Eager user-process memory management.
//!
//! Owns the per-process address-space bookkeeping (region table) and the
//! commit/release primitives used by the loader and the ring-3 `brk`/`mmap`/
//! `munmap` syscalls.
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

/// 4 KiB page.
pub const PAGE: u64 = 4096;

/// Default per-process committed frame budget (256 MiB), the hard ceiling for
/// `brk` + `mmap` combined. The physical allocator is the final arbiter — this
/// is an additional guard so one runaway process cannot starve every driver.
const USER_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

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
    pub flags: PageFlags,
    /// True when ≥1 unmapped page must protect this region's underside.
    pub guard: bool,
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
    let asp = AddressSpace {
        root,
        brk_floor: image_top,
        brk_cur: image_top,
        stack_top,
        committed,
        budget_pages: USER_BUDGET_BYTES / PAGE,
        regions: vec![
            Region {
                kind: RegionKind::Image,
                vaddr: image_floor,
                pages: (image_top - image_floor) / PAGE,
                flags: PageFlags::USER | PageFlags::READ | PageFlags::WRITE,
                guard: false,
            },
            Region {
                kind: RegionKind::Stack,
                vaddr: stack_base,
                pages: 8,
                flags: stack_flags,
                guard: true,
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

    let new = align_up(new_break);
    if new == 0 || new < asp.brk_floor {
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
        let npages = (new - old) / PAGE;
        if npages > asp.budget_pages.saturating_sub(asp.committed) {
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
            }),
        }
    } else {
        // Shrink: release committed frames below the new break.
        let npages = (old - new) / PAGE;
        let mut vmm = Vmm::from_root(asp.root);
        release_pages(&mut vmm, alloc, new, npages);
        asp.brk_cur = new;
        asp.committed -= npages;
        let heap_pages = (new - asp.brk_floor) / PAGE;
        if heap_pages == 0 {
            asp.regions.retain(|r| r.kind != RegionKind::Heap);
        } else if let Some(h) = asp.regions.iter_mut().find(|r| r.kind == RegionKind::Heap) {
            h.pages = heap_pages;
        }
    }
    Ok(asp.brk_cur)
}

/// `mmap(addr, len, prot)`: eagerly commit `len` zeroed anonymous pages.
///
/// `addr == 0` picks the first available gap above the break (page-aligned,
/// guard page between regions, capped by the user stack's guard). A fixed
/// `addr` must be page-aligned and free. `len` must be page-aligned and > 0.
pub fn mmap(
    idx: usize,
    addr: u64,
    len: u64,
    prot: u64,
    alloc: &mut BitmapAllocator,
) -> Result<u64, i64> {
    let mut table = ADDR_SPACES.lock();
    let Some(asp) = table.slots.get_mut(idx).and_then(|s| s.as_mut()) else {
        return Err(EFAULT);
    };
    if len == 0 || len % PAGE != 0 {
        return Err(EINVAL);
    }
    let npages = len / PAGE;
    let flags = prot_to_flags(prot)?;

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

    if npages > asp.budget_pages.saturating_sub(asp.committed) {
        return Err(ENOMEM);
    }
    let mut vmm = Vmm::from_root(asp.root);
    if commit_pages(&mut vmm, alloc, vaddr, npages, flags).is_err() {
        return Err(ENOMEM);
    }
    asp.committed += npages;
    asp.regions.push(Region {
        kind: RegionKind::Anon,
        vaddr,
        pages: npages,
        flags,
        guard: true,
    });
    Ok(vaddr)
}

/// `munmap(addr, len)`: release and unmap one or more whole anonymous regions.
///
/// `[addr, addr+len)` must exactly cover a run of contiguous `Anon` regions.
/// Partial-region unmapping (and any attempt on the image, stack or heap) is
/// rejected with `EINVAL`; the heap shrinks through `brk` instead.
pub fn munmap(idx: usize, addr: u64, len: u64, alloc: &mut BitmapAllocator) -> Result<(), i64> {
    let mut table = ADDR_SPACES.lock();
    let Some(asp) = table.slots.get_mut(idx).and_then(|s| s.as_mut()) else {
        return Err(EFAULT);
    };
    if len == 0 || len % PAGE != 0 || addr % PAGE != 0 {
        return Err(EINVAL);
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return Err(EINVAL),
    };

    // Collect the anon regions that tile `[addr, end)` exactly.
    let mut doomed: Vec<usize> = Vec::new();
    let mut cursor = addr;
    while cursor < end {
        let mut hit = None;
        for (i, r) in asp.regions.iter().enumerate() {
            if r.kind == RegionKind::Anon && r.vaddr == cursor {
                hit = Some(i);
                break;
            }
        }
        let Some(i) = hit else { return Err(EINVAL) };
        let reg_end = asp.regions[i].end();
        if reg_end > end {
            return Err(EINVAL); // partial region: use brk for the heap, not here
        }
        doomed.push(i);
        cursor = reg_end;
    }

    let mut vmm = Vmm::from_root(asp.root);
    let mut freed = 0u64;
    for &i in &doomed {
        let r = asp.regions[i];
        release_pages(&mut vmm, alloc, r.vaddr, r.pages);
        freed += r.pages;
    }
    doomed.sort_unstable_by(|a, b| b.cmp(a));
    for i in doomed {
        asp.regions.remove(i);
    }
    asp.committed -= freed;
    Ok(())
}

// ── Commit / release primitives ──────────────────────────────────────

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
    let mut mapped: Vec<u64> = Vec::new();
    for _ in 0..npages {
        let Some(phys) = alloc.alloc() else {
            for &c in mapped.iter().rev() {
                if let Some(p) = vmm.translate(c) {
                    vmm.unmap_4k(alloc, c);
                    unsafe { alloc.free(p); }
                }
            }
            return Err(());
        };
        unsafe {
            core::ptr::write_bytes(to_physmap(phys) as *mut u8, 0, PAGE as usize);
        }
        vmm.map_4k(alloc, va, phys, flags);
        mapped.push(va);
        va += PAGE;
    }
    Ok(())
}

/// Unmap and free every frame backing `[vaddr, vaddr + npages*PAGE)`.
///
/// `Vmm::unmap_4k` flushes the local TLB and broadcasts a cross-CPU shootdown
/// before any frame (leaf or intermediate table) is released — required here
/// because the process root is the *active* CR3 during a syscall.
fn release_pages(vmm: &mut Vmm, alloc: &mut BitmapAllocator, vaddr: u64, npages: u64) {
    let mut va = vaddr;
    for _ in 0..npages {
        if let Some(phys) = vmm.translate(va) {
            vmm.unmap_4k(alloc, va);
            unsafe { alloc.free(phys); }
        }
        va += PAGE;
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

const fn align_up(x: u64) -> u64 {
    (x + (PAGE - 1)) & !(PAGE - 1)
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

/// First-fit page-aligned gap starting one guard page above the break.
fn find_fit(asp: &AddressSpace, npages: u64) -> Option<u64> {
    let len = npages * PAGE;
    let ceiling = user_ceiling(asp);
    let mut cand = asp.brk_cur.checked_add(PAGE)?;

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
    let exec = prot & 4 != 0;
    let write = prot & 2 != 0;
    if exec && write {
        return Err(EINVAL); // W^X: an executable mapping is never writable
    }
    let mut f = PageFlags::USER | PageFlags::READ;
    if exec {
        f |= PageFlags::EXECUTE;
    } else if write {
        f |= PageFlags::WRITE;
    }
    Ok(f)
}