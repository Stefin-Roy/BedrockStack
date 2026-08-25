//! User page-fault resolution — demand paging and copy-on-write (x86_64).
//!
//! # Policy (amends INV-UM-01; see `Invariants/invariants-26-user-mem.md`)
//! A ring-3 #PF allocates in exactly two cases:
//!
//! 1. **Demand fill** — a not-present fault inside a *writable*
//!    `Heap`/`Stack`/`Anon` region maps a zeroed frame charged against the
//!    dynamic budget. `Image` regions are excluded: their holes were never
//!    mapped by the loader and stay fatal.
//! 2. **Copy-on-write** — a write fault on a present non-writable leaf whose
//!    owning region is writable. Sole-owner frames upgrade in place; shared
//!    frames get a private copy (fork siblings keep theirs).
//!
//! Every other fault is a real access violation: the caller kills the task.
//! A resolution returning `true` retries the faulting instruction by plain
//! return from the IDT handler.
//!
//! # Locking
//! Order is always `ADDR_SPACES` → VMM (the order `brk`/`mmap` established);
//! nothing acquires them the other way around, so no inversion is possible.
//! The resolver runs on the faulting CPU with that task's CR3 active; leaf
//! edits invalidate locally (`invlpg`). Cross-CPU staleness for the same root
//! is impossible under the BSP-only scheduler (only this CPU ever has this
//! root resident — INV-FC-02), so no shootdown is needed here.

use x86_64::structures::idt::PageFaultErrorCode;

/// Try to resolve a ring-3 page fault at `cr2`. Returns `true` when the
/// faulting instruction should simply be retried; `false` means the fault is
/// unrecoverable and the task must die.
///
/// Fetches the physical allocator itself — safe because a ring-3 fault can
/// only happen long after the allocator is live.
pub fn resolve_user_fault(cr2: u64, err: PageFaultErrorCode) -> bool {
    if err.contains(PageFaultErrorCode::MALFORMED_TABLE) {
        return false;
    }
    let Some(vm) = crate::task::current_vm() else {
        return false;
    };
    let write = err.contains(PageFaultErrorCode::CAUSED_BY_WRITE);
    let protection = err.contains(PageFaultErrorCode::PROTECTION_VIOLATION);

    if !protection {
        // Absent page. Fill regardless of read/write: lazily-committed
        // regions (and any future MAP_LAZY user) fault on first touch of
        // either kind.
        return crate::mm::usermem::demand_fill(vm, cr2 & !0xFFF);
    }

    if !write {
        // Execute/read violation on a present page: real permission breach.
        return false;
    }

    cow_resolve(vm, cr2)
}

/// Copy-on-write resolution for a write-protection fault at `va`.
fn cow_resolve(vm: usize, va: u64) -> bool {
    use crate::mm::layout::to_physmap;
    use crate::mm::vmm::translate_user;

    let Some((root, region_writable)) = crate::mm::usermem::cow_context(vm, va) else {
        return false;
    };
    if !region_writable {
        // PROT_READ mapping written: real violation, not a leftover COW mark.
        return false;
    }

    let Some((phys, user_ok, leaf_writable)) = translate_user(root, va) else {
        // Present-per-error-code but walk says absent: stale TLB edge —
        // retrying will take a clean not-present fault into demand_fill.
        return false;
    };
    if !user_ok || leaf_writable {
        // Kernel-only leaf, or the TLB raced ahead of our own edit: retriable.
        return true;
    }

    let old = phys & !0xFFF;
    if crate::mm::framecnt::is_sole_owner(old) {
        // Last owner: upgrade in place, normalize the counter back to
        // untracked so a later fork's 0→2 CAS works.
        crate::mm::framecnt::privatize(old);
        crate::mm::vmm::user_leaf_make_writable(root, va);
        return true;
    }

    // Shared: private copy. The new frame starts untracked — sole property
    // of this address space from here on.
    let Some(new) = crate::mm::heap::get_phys_allocator_mut().alloc() else {
        return false; // ENOMEM under pressure → kill (documented policy)
    };
    unsafe {
        core::ptr::copy_nonoverlapping(
            to_physmap(old) as *const u8,
            to_physmap(new) as *mut u8,
            4096,
        );
    }
    crate::mm::vmm::user_leaf_repoint_writable(root, va, new);
    // Drop our reference on the shared frame; the sibling(s) still hold it.
    crate::mm::framecnt::decref_or_free(crate::mm::heap::get_phys_allocator_mut(), old);
    true
}
