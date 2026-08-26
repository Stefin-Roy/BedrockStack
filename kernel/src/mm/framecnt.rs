//! Frame reference counts for shared (copy-on-write) user frames.
//!
//! # Representation
//! One `u32` per physical frame, indexed by `phys >> 12` (the bitmap
//! allocator indexes its own bitmap the same way), lazily allocated from the
//! kernel heap on the first [`share_frame`] — nothing is paid until the first
//! fork actually shares something.
//!
//! # Counting convention
//! An entry of `0` means "untracked": the frame has exactly one owner and
//! behaves exactly like an ordinary eager-commit page (teardown frees it
//! outright). The first additional owner bumps `0 → 2`; every further owner
//! adds 1. Therefore "sole owner" ⇔ entry ∈ `{0, 1}`; "shared" ⇔ ≥ 2.
//!
//! # Race model
//! Fork (`share_frame`) runs under `ADDR_SPACES` (`IrqMutex`) against a *live*
//! parent task, and teardown only ever happens from that space's own exit path
//! (idle `reap_dead` on kernel root, single global FIFO; deadline-only, no
//! periodic LAPIC — the ISR touches only atomics). The cooperative BSP-only
//! scheduler keeps incref/decref pairs ordered in practice, and the CAS loop
//! remains correct even if two forks ever race: atomics guarantee a free
//! only happens on observing exactly 1.
//!
//! INVARIANT (INV-FC-01): every *leaf* frame reachable from more than one
//! address-space root MUST have been passed through [`share_frame`] by the
//! clone path before the second root becomes schedulable. Intermediate page
//! tables are never tracked — they are strictly per-root.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use spin::Once;

use crate::mm::phys_alloc::BitmapAllocator;

static TABLE: Once<&'static [AtomicU32]> = Once::new();

/// Build the table once. Requires the kernel heap and the physical allocator
/// to be live (any post-init call site satisfies this — fork cannot run
/// earlier).
fn table() -> &'static [AtomicU32] {
    TABLE.call_once(|| {
        let alloc = unsafe { &*crate::mm::heap::phys_allocator_raw() };
        let n = alloc.total_frames();
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(AtomicU32::new(0));
        }
        Vec::leak(v)
    })
}

#[inline]
fn slot(t: &'static [AtomicU32], phys: u64) -> Option<&'static AtomicU32> {
    let i = (phys >> 12) as usize;
    t.get(i)
}

/// Record one additional owner of `phys`. Called by the fork path once per
/// shared leaf, *before* the child's root becomes visible to anything that
/// can fault on it (INV-FC-01).
///
/// Implemented as a CAS loop rather than a `0→2` CAS + unconditional
/// `fetch_add`: under a preemptive scheduler (INV-FC-02 no longer assumed)
/// two forks can race on the same frame, and `fetch_add` could bump the
/// counter of a frame whose `decref_or_free` had just observed `1 → 0` and
/// freed it — resurrecting a live allocation. The loop re-reads after every
/// failed CAS, so a transition is only ever taken from a freshly observed
/// value: `0 → 2` (first share) or `cur → cur+1` (further shares), never an
/// increment through a window where the frame was freed.
pub fn share_frame(phys: u64) {
    let t = table();
    let Some(e) = slot(t, phys) else { return };
    loop {
        let cur = e.load(Ordering::Acquire);
        if cur == 0 {
            if e.compare_exchange_weak(0, 2, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        } else {
            debug_assert!(cur != u32::MAX, "share_frame refcount overflow on {phys:#x}");
            if e.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        }
    }
}

/// True when the caller's mapping is the only remaining reference. Untracked
/// (never-shared) frames are trivially sole-owner.
pub fn is_sole_owner(phys: u64) -> bool {
    match TABLE.get().and_then(|t| slot(t, phys)) {
        None => true,
        Some(e) => matches!(e.load(Ordering::Acquire), 0 | 1),
    }
}

/// Drop one reference; free the frame when the last owner releases it.
///
/// Untracked frames (entry `0`) free immediately — identical to the raw
/// `alloc.free` behaviour every teardown path had before CoW existed, which is
/// what lets *all* leaf-free call sites route through here uniformly.
pub fn decref_or_free(alloc: &BitmapAllocator, phys: u64) {
    let Some(e) = TABLE.get().and_then(|t| slot(t, phys)) else {
        unsafe { alloc.free(phys) };
        return;
    };
    match e.try_update(Ordering::AcqRel, Ordering::Acquire, |c| {
        if c == 0 { None } else { Some(c - 1) }
    }) {
        // Was untracked: sole owner since allocation.
        Err(_) => unsafe { alloc.free(phys) },
        // We took the last tracked reference.
        Ok(1) => unsafe { alloc.free(phys) },
        Ok(_) => {}
    }
}

/// Drop one fork-share during a failed clone's unwind. The parent always
/// still owns the frame, so this never frees: decrement, and when the counter
/// reaches `1` (only the untracked parent remains) normalize to `0` so the
/// frame returns to the "untracked single owner" state exactly as before the
/// aborted share — a later fork's `0 → 2` CAS works again.
pub fn unshare(phys: u64) {
    let Some(e) = TABLE.get().and_then(|t| slot(t, phys)) else { return };
    match e.try_update(Ordering::AcqRel, Ordering::Acquire, |c| {
        if c >= 2 { Some(c - 1) } else { None }
    }) {
        Ok(2) => {
            // Only the parent's implicit reference is left: untrack.
            let _ = e.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        _ => {}
    }
}

/// Normalize a sole-owner tracked entry (`1`) back to untracked (`0`) after a
/// copy-on-write upgrade-in-place, so a future fork's `0 → 2` CAS works.
pub fn privatize(phys: u64) {
    if let Some(e) = TABLE.get().and_then(|t| slot(t, phys)) {
        let _ = e.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}
