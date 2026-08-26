//! Cooperative task scheduler.
//!
//! A single global FIFO run queue of kernel tasks is switched by `switch_to`.
//! The scheduler runs on the BSP only (APs idle in `ap_entry64`), so no
//! per-CPU queues or cross-CPU locking are needed yet.  Tasks are
//! `Box::leak`ed to `&'static mut`; exiting tasks park as zombies in `ZOMBIES`
//! (retaining their exit code and `/proc` directory) until a parent's `:wait`
//! consumes them or their own parent dies, and `reap_dead` frees their root
//! page tables, kernel stacks, and task boxes from the idle loop.  A parent
//! that wants a child's exit code parks itself in `WAITERS` (the same pattern
//! as `SLEEPING`).  The idle anchor is the run()/idle context, captured on the
//! first switch away and restored when the queue empties.
//!
//! User-mode entry (`enter_userspace`) builds an iretq frame and an initial
//! context pointing at `user_iret`.

pub mod load;
mod switch;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::drivers::serial::SerialPort;
use crate::filesystems::vfs::irq::IrqMutex;
use crate::mm::layout::{KSTACK_SIZE, KSTACK_VADDR_BASE, MAX_KSTACKS};
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};
use hashbrown::HashMap;
use spin::Once;
use switch::switch_to;
pub use switch::{TaskContext, user_iret_addr};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Ready,
    Running,
    ZzZ,
    Dead,
}

/// Scheduling class. `Interactive` runs a shorter slice but is picked up to
/// `WEIGHT_INTERACTIVE` times per single `Batch` dispatch (weighted
/// round-robin, see `pick_next`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Priority {
    Interactive,
    Batch,
}

/// Time slice granted per dispatch, in ns.
pub const SLICE_INTERACTIVE_NS: u64 = 4_000_000;
pub const SLICE_BATCH_NS: u64 = 12_000_000;
/// WRR budget: Interactive is dispatched this many times per Batch round.
const WEIGHT_INTERACTIVE: u32 = 3;

impl Priority {
    #[inline]
    pub fn slice_ns(self) -> u64 {
        match self {
            Priority::Interactive => SLICE_INTERACTIVE_NS,
            Priority::Batch => SLICE_BATCH_NS,
        }
    }
}

/// A schedulable unit of execution.
pub struct Task {
    pub id: u64,
    pub state: TaskState,
    /// Scheduling class (WRR weight + slice length). Never mutated after spawn.
    pub prio: Priority,
    pub kernel_stack_top: u64,
    /// Index into the fixed task-stack window (see `alloc_kernel_stack`),
    /// `usize::MAX` when the task has no kernel stack of its own (the idle
    /// anchor).  Freed by `reap_dead`.
    pub kstack_slot: usize,
    /// Index into the eager user-memory table (`mm::usermem`), or `0` when the
    /// task has no user address space.  Set by `enter_userspace`/`:spawn` and
    /// released by `reap_dead`.
    pub vm: usize,
    /// Page-table root (CR3) this task runs in.
    pub root: u64,
    pub user_gs: u64,
    pub ctx: TaskContext,
    /// Exit code retained while the task parks as a zombie (set by
    /// `exit_current`, or `KILLED_EXIT_CODE` for a killed/faulted task),
    /// consumed by a parent's `:wait`.
    pub exit_code: u64,
    /// The pid of the task that spawned this one (0 for kernel-launched tasks
    /// such as INIT).  Only a task's parent may `:wait` on it.
    pub parent_pid: u64,
    /// Command-line arguments passed through `:spawn`'s `{path, args}` input,
    /// readable by the task itself via `/proc/self/args`.  No entry-point ABI
    /// change — the program fetches them like any other object.
    pub args: String,
    /// Capability supervisor page: physical frame backing `CAP_SLOT_VA` (supervisor-only,
    /// READ, no USER) in this task's PML4. `0` when none.
    pub caps_phys: u64,
    /// Authoritative in-kernel capability set, Arc-shared. `current_caps()`
    /// snapshots it with a single refcount bump; grants go through
    /// `Arc::make_mut` copy-on-write. `None` = no caps yet (bypass for kernel
    /// tasks with `vm == 0`, deny-all for user tasks).
    pub caps_arc: Option<Arc<Vec<crate::caps::Cap>>>,
    /// Number of 4K pages mapped at CAP_SLOT_VA for the supervisor mirror (0 when none).
    pub caps_pages: usize,
    /// Base VA of this task's supervisor caps window. Randomized per process
    /// (`mm::layout::pick_caps_va`); `CAP_SLOT_VA` is only the legacy default
    /// used until a mirror is first installed.
    pub caps_slot_va: u64,
    /// PKU rights register snapshot (2 bits per key; 0 = all accessible).
    /// Applied on context switch and restored at syscall exit — the kernel
    /// itself always works with PKRU=0 (`pku_enter`). x86_64 only.
    pub pkru: u32,
}

impl Task {
    pub const fn new(kernel_stack_top: u64, root: u64, user_gs: u64, ctx: TaskContext) -> Self {
        Task {
            id: 0,
            state: TaskState::Ready,
            prio: Priority::Interactive,
            kernel_stack_top,
            kstack_slot: usize::MAX,
            vm: 0,
            root,
            user_gs,
            ctx,
            exit_code: 0,
            parent_pid: 0,
            args: String::new(),
            caps_phys: 0,
            caps_arc: None,
            caps_pages: 0,
            caps_slot_va: crate::mm::layout::CAP_SLOT_VA,
            pkru: 0,
        }
    }
}

// ── Durable lineage tracking ────────────────────────────────────────────
// Keep pid→parent for every spawn, even after Task is reaped, so that
// `B→A→Z→Y→X` can be walked through dead `A`. GC removes entries that are
// no longer ancestors of any live Task.
static LINEAGE: Once<IrqMutex<HashMap<u64, u64>>> = Once::new();
fn lineage_map() -> &'static IrqMutex<HashMap<u64, u64>> {
    LINEAGE.call_once(|| IrqMutex::new_keyed(LOCK_CLASS_PARK, HashMap::new()))
}
pub fn lineage_insert(pid: u64, parent: u64) {
    lineage_map().lock().insert(pid, parent);
}
fn lineage_chain(mut pid: u64) -> Vec<u64> {
    let map = lineage_map().lock();
    let mut out = Vec::new();
    let mut seen = hashbrown::HashSet::new();
    // Unbounded walk until 0 or missing; cycle-guarded via HashSet.
    while seen.insert(pid) {
        if let Some(&ppid) = map.get(&pid) {
            if ppid == 0 { break; }
            out.push(ppid);
            pid = ppid;
        } else {
            break;
        }
    }
    out
}
/// True if `ancestor` is an ancestor of `descendant` via durable lineage (including direct parent).
/// Used by caps to allow `proc/<pid>/...` alias to `proc/self/...` when caller is ancestor.
pub fn is_ancestor(ancestor: u64, descendant: u64) -> bool {
    if ancestor == descendant {
        return true;
    }
    let map = lineage_map().lock();
    let mut cur = descendant;
    let mut seen = hashbrown::HashSet::new();
    while seen.insert(cur) {
        if let Some(&ppid) = map.get(&cur) {
            if ppid == ancestor {
                return true;
            }
            if ppid == 0 {
                break;
            }
            cur = ppid;
        } else {
            break;
        }
    }
    false
}

/// Current task pid, if any.
pub fn current_pid() -> Option<u64> {
    let pc = crate::smp::current_per_cpu();
    let ptr = pc.current_task.load(Ordering::Relaxed);
    if ptr.is_null() {
        return None;
    }
    let t = unsafe { &*(ptr as *const Task) };
    Some(t.id)
}

fn lineage_gc() {
    // Collect live pids (those with a Task still allocated)
    let mut live: Vec<u64> = Vec::new();
    if let Some(t) = CURRENT.lock().as_ref() { live.push(t.id); }
    for t in QUEUE.lock().iter() { live.push(t.id); }
    for (_, t) in SLEEPING.lock().iter() { live.push(t.id); }
    for (t, _) in WAITERS.lock().iter() { live.push(t.id); }
    for t in ZOMBIES.lock().iter() { live.push(t.id); }
    for t in RECLAIM.lock().iter() { live.push(t.id); }
    // Mark all ancestors of live pids as needed (unbounded, cycle-guarded)
    let mut needed = hashbrown::HashSet::new();
    {
        let map = lineage_map().lock();
        for &pid in &live {
            let mut cur = pid;
            let mut seen = hashbrown::HashSet::new();
            while seen.insert(cur) {
                if let Some(&ppid) = map.get(&cur) {
                    if ppid == 0 { break; }
                    needed.insert(ppid);
                    cur = ppid;
                } else { break; }
            }
        }
    }
    // Remove entries where key not live and not needed as ancestor
    let mut map = lineage_map().lock();
    let keys: Vec<u64> = map.keys().copied().collect();
    for k in keys {
        let is_live = live.contains(&k);
        let is_needed = needed.contains(&k);
        if !is_live && !is_needed {
            map.remove(&k);
        }
    }
}

/// Number of 4 KiB frames backing one kernel stack.
const KSTACK_PAGES: usize = (KSTACK_SIZE as usize) / 4096;

// ── lockdep order classes for the scheduler locks ──────────────────────
//
// Encodes the prose ordering rules from the lock doc-comments into machine-
// checked keys (`lockdep` feature; see `smp::lockdep`). Strictly ascending
// acquisition is required; equal classes are never nestable.
//
//   KSTACK_IN_USE (1) < QUEUE (2) < CURRENT (3) < park lists (4)
//
// * KSTACK_IN_USE: leaf — taken on its own by alloc/free_kernel_stack.
// * QUEUE before CURRENT: `schedule` locks CURRENT to store/drop the
//   dispatched task while QUEUE's guard is live (dispatch and park paths);
//   the reverse never nests (`CURRENT.lock().take()` releases before
//   QUEUE is acquired).
// * Park lists (SLEEPING/WAITERS/ZOMBIES/RECLAIM/LINEAGE): per their doc
//   comments they are never held together with each other — equal class
//   makes any such nesting panic under lockdep instead of resting on
//   convention. QUEUE→park-list direction (e.g. schedule parking a Dead
//   task into ZOMBIES after dropping q) stays legal since 2 < 4.
const LOCK_CLASS_KSTACK: u32 = 1;
const LOCK_CLASS_QUEUE: u32 = 2;
const LOCK_CLASS_CURRENT: u32 = 3;
const LOCK_CLASS_PARK: u32 = 4;

/// Liveness bitmap for the fixed task-stack window slots.
/// IrqMutex: `alloc_kernel_stack`/`free_kernel_stack` may run while a deadline
/// (universal_timer one-shot, never periodic LAPIC) wakes sleepers; IRQs
/// disabled avoids `QUEUE`/`SLEEPING` re-entry deadlock (single global FIFO,
/// SCHED-L001).
static KSTACK_IN_USE: IrqMutex<[bool; MAX_KSTACKS]> =
    IrqMutex::new_keyed(LOCK_CLASS_KSTACK, [false; MAX_KSTACKS]);

/// Allocate the next free slot in the fixed task-stack window, mapping its
/// four frames into the kernel root, and return `(stack_top, slot)`.
/// Returns `None` when all slots are taken or the allocator is exhausted.
///
/// The window sits inside PML4 slot 511, whose subtree is established before
/// the first `clone_high_half` (the heap maps the same slot during `init`), so
/// a stack mapped into the kernel root is visible under every task root —
/// including roots cloned after this allocation.  Sharing, not snapshotting,
/// is the mechanism, so there is no post-clone heap-growth hazard.
pub fn alloc_kernel_stack(alloc: &mut BitmapAllocator) -> Option<(u64, usize)> {
    let mut in_use = KSTACK_IN_USE.lock();
    let slot = in_use.iter().position(|&b| !b)?;
    let base = KSTACK_VADDR_BASE - (slot as u64) * KSTACK_SIZE;
    let mut vmm = Vmm::from_root(kernel_root());
    // Track allocated frames for rollback on OOM mid-way (S7).
    let mut allocated: [u64; KSTACK_PAGES] = [0; KSTACK_PAGES];
    let mut allocated_len = 0usize;
    for i in 0..KSTACK_PAGES {
        let pa = match alloc.alloc() {
            Some(p) => p,
            None => {
                // Roll back any pages already mapped for this slot.
                for j in 0..allocated_len {
                    let va = base + (j as u64) * 4096;
                    let mut rollback_vmm = Vmm::from_root(kernel_root());
                    // Collect and free via unmap_range path (TLB shootdown needed)
                    let mut frames: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
                    rollback_vmm.unmap_range_collect(alloc, va, 4096, &mut frames);
                    for phys in &frames { unsafe { alloc.free(*phys); } }
                    // Also free if translate missed due to supervisor flag? fallback free original.
                    // The frame was zeroed but not yet freed, free the original pa directly if not in frames.
                    let orig = allocated[j];
                    if !frames.contains(&orig) { unsafe { alloc.free(orig); } }
                }
                return None;
            }
        };
        unsafe {
            core::ptr::write_bytes(crate::mm::layout::to_physmap(pa) as *mut u8, 0, 4096);
        }
        let va = base + (i as u64) * 4096;
        vmm.map_4k(alloc, va, pa, PageFlags::READ | PageFlags::WRITE);
        allocated[allocated_len] = pa;
        allocated_len += 1;
    }
    // Assert KSTACK window's PML4 slot (511) is now present and shared. If
    // this were a new higher-half slot, `map_4k` already called
    // `sync_clone_half` (VMM) so clones share the PDPT. Debug-only check.
    debug_assert!(Vmm::from_root(kernel_root()).translate(base).is_some());
    in_use[slot] = true;
    Some((base + KSTACK_SIZE, slot))
}

/// Allocate and map a capability supervisor window for `root` at `va`
/// (supervisor-only READ, no USER). Returns the base physical frame
/// (2 contiguous 4K). The caller fills it via `caps::serialize_to_page`.
pub fn alloc_caps_page(root: u64, va: u64, alloc: &mut BitmapAllocator) -> Option<u64> {
    let pages = (crate::mm::layout::CAP_SLOT_SIZE as usize) / 4096;
    let phys = alloc.alloc_contiguous(pages)?;
    for i in 0..pages {
        unsafe {
            core::ptr::write_bytes(crate::mm::layout::to_physmap(phys + i as u64 * 4096) as *mut u8, 0, 4096);
        }
    }
    let mut vmm = Vmm::from_root(root);
    for i in 0..pages {
        vmm.map_4k(alloc, va + i as u64 * 4096, phys + i as u64 * 4096, PageFlags::READ);
    }
    Some(phys)
}

/// Unmap and free the capability supervisor window at `va` for `root`.
pub fn free_caps_page(root: u64, va: u64, alloc: &mut BitmapAllocator) {
    let mut vmm = Vmm::from_root(root);
    let mut frames: Vec<u64> = Vec::new();
    vmm.unmap_range_collect(
        alloc,
        va,
        crate::mm::layout::CAP_SLOT_SIZE,
        &mut frames,
    );
    let expected = (crate::mm::layout::CAP_SLOT_SIZE as usize) / 4096;
    debug_assert!(frames.len() <= expected, "caps window should be at most {} pages", expected);
    for p in frames {
        unsafe { alloc.free(p) };
    }
}

/// Serialize caps to a supervisor mirror page and map it into `root` at `va`.
/// Returns the physical frame (2 pages, see `alloc_caps_page`). The caller
/// owns the authoritative set as an `Arc<Vec<Cap>>` on the Task.
pub fn install_caps(
    root: u64,
    caps: &[crate::caps::Cap],
    va: u64,
    alloc: &mut BitmapAllocator,
) -> Option<u64> {
    let phys = alloc_caps_page(root, va, alloc)?;
    crate::caps::serialize_to_page(caps, phys);
    Some(phys)
}

/// Grant a capability to the task with `pid`, allocating a supervisor page if needed.
/// Returns `Ok` if granted or already present, `Err` if OOM or invalid.
pub fn grant_cap_to_pid(pid: u64, cap: crate::caps::Cap) -> Result<(), crate::unispace::UnispaceError> {
    crate::caps::validate_cap(&cap)?;
    // Helper to try to grant to a &mut Task
    fn do_grant(task: &mut Task, cap: &crate::caps::Cap) -> Result<(), crate::unispace::UnispaceError> {
        if task.caps_arc.is_none() {
            let alloc = crate::mm::heap::get_phys_allocator_mut();
            let new_caps = Vec::new();
            if let Some(phys) = install_caps(task.root, &new_caps, task.caps_slot_va, alloc) {
                task.caps_phys = phys;
                task.caps_arc = Some(Arc::new(new_caps));
            } else {
                return Err(crate::unispace::UnispaceError::OutOfMemory);
            }
        }
        let arc = match task.caps_arc.as_mut() {
            Some(a) => a,
            None => return Err(crate::unispace::UnispaceError::OutOfMemory),
        };
        let caps = Arc::make_mut(arc);
        if caps.len() >= crate::caps::MAX_CAPS_PER_TASK {
            return Err(crate::unispace::UnispaceError::OutOfMemory);
        }
        for c in caps.iter_mut() {
            if c.path == cap.path && c.method == cap.method {
                if c.perm.covers(cap.perm) {
                    return Ok(());
                } else {
                    c.perm = cap.perm;
                    if task.caps_phys != 0 {
                        crate::caps::serialize_to_page(caps, task.caps_phys);
                    }
                    return Ok(());
                }
            }
        }
        caps.push(cap.clone());
        if task.caps_phys != 0 {
            crate::caps::serialize_to_page(caps, task.caps_phys);
        }
        Ok(())
    }

    match with_task_mut(pid, |t| Some(do_grant(t, &cap))) {
        Some(r) => r,
        None => Err(crate::unispace::UnispaceError::NotFound),
    }
}

/// Propagate a newly granted capability from the current task up through its parent chain.
/// The current task already gets the cap via `grant_to_current`; this walks ancestors and grants each.
/// Uses durable lineage map so dead intermediates do not break the walk to X.
pub fn propagate_cap_to_parents(path: String, method: Option<String>, perm: crate::caps::Perm) {
    // First grant to current (if not already)
    let _ = crate::caps::grant_to_current(path.clone(), method.clone(), perm);
    // Walk durable lineage chain
    let pid_opt = {
        let pc = crate::smp::current_per_cpu();
        let ptr = pc.current_task.load(Ordering::Relaxed);
        if ptr.is_null() {
            None
        } else {
            let t = unsafe { &*(ptr as *const Task) };
            Some(t.id)
        }
    };
    let Some(start) = pid_opt else { return; };
    let chain = lineage_chain(start);
    for ppid in chain {
        let cap = crate::caps::Cap { path: path.clone(), method: method.clone(), perm };
        let _ = grant_cap_to_pid(ppid, cap);
    }
}

pub(crate) fn free_kernel_stack(slot: usize, alloc: &mut BitmapAllocator) {
    if slot >= MAX_KSTACKS {
        return;
    }
    // Targets the kernel root explicitly (`Vmm::from_root(kernel_root())`);
    // the current CR3 is irrelevant, so this is legal both from `reap_dead`
    // on the idle/kernel root AND from fork/spawn rollback paths running on
    // a live user root (same pattern as `alloc_kernel_stack`). KERNEL_ROOT
    // is stored by `task::init` before any task can spawn or be reaped.
    debug_assert!(
        kernel_root() != 0,
        "free_kernel_stack: task::init not yet run"
    );
    let mut in_use = KSTACK_IN_USE.lock();
    if !in_use[slot] {
        return;
    }
    in_use[slot] = false;
    let base = KSTACK_VADDR_BASE - (slot as u64) * KSTACK_SIZE;
    let mut vmm = Vmm::from_root(kernel_root());
    let mut frames: Vec<u64> = Vec::new();
    vmm.unmap_range_collect(alloc, base, KSTACK_PAGES as u64 * 4096, &mut frames);
    for phys in frames {
        unsafe {
            alloc.free(phys);
        }
    }
}

/// Global FIFO of runnable tasks. IrqMutex: deadline wake (one-shot universal_timer,
/// never periodic LAPIC — SCHED-L002 ISR touch-nothing) may set `need_resched`
/// via atomics while a task holds `QUEUE`; IRQs disabled avoids deadlock.
static QUEUE: IrqMutex<VecDeque<&'static mut Task>> =
    IrqMutex::new_keyed(LOCK_CLASS_QUEUE, VecDeque::new());

/// The task currently running on this CPU, or `None` when idle.
static CURRENT: IrqMutex<Option<&'static mut Task>> = IrqMutex::new_keyed(LOCK_CLASS_CURRENT, None);

/// Parked dead tasks (zombies) awaiting a parent's `:wait` or the death of
/// their own parent.
///
/// Pushed at every switch-away site for a Dead task, then drained by
/// `reap_dead` from the idle loop.  A dead task cannot free its own stack
/// (its CTX still points into it and the CPU parked on it), so teardown is
/// deferred until the scheduler is back on the idle stack.  A zombie is kept
/// while its parent is still live (the parent may `:wait` it) and freed once
/// its parent is gone.  `reap_dead` reads the other scheduler lists only while
/// holding this one, and nothing acquires this lock while holding those, so
/// there is no ordering cycle. IrqMutex for deadline-only `need_resched`.
static ZOMBIES: IrqMutex<Vec<&'static mut Task>> = IrqMutex::new_keyed(LOCK_CLASS_PARK, Vec::new());

/// Consumed zombies awaiting idle teardown: their exit code was collected by
/// a parent's `:wait`, and `reap_dead` frees their user page tables, kernel
/// stacks, `/proc` entries, and task boxes from the idle loop.
///
/// Teardown must not run from a user task's context — `destroy_root` issues a
/// TLB shootdown whose APIC MMIO access requires the kernel root's address
/// space (a user root does not map the APIC).  A consumed zombie is moved
/// here (and is thus invisible to every scheduler scan), so the `:wait` and
/// `/proc` paths treat it as already reaped, and only `reap_dead` — which runs
/// on the idle stack under the kernel root — drains this queue.
static RECLAIM: IrqMutex<Vec<&'static mut Task>> =
    IrqMutex::new_keyed(LOCK_CLASS_PARK, Vec::new());

/// Parked tasks awaiting a child's exit (`/proc/self:wait`), as `(task, pid)`
/// keyed by the target pid.
///
/// A waiter is removed from the run queue at park time and lives here until
/// its target parks into `ZOMBIES` — `park_zombie` then moves it back to
/// `QUEUE` as `Ready`, where its resumed `:wait` activation consumes the
/// zombie.  Never held together with `QUEUE`: `wake_waiters_for` drops the
/// waiter list before locking the queue, and no `:wait` activation parks
/// while holding it.  Only one task can wait on a given child (its parent),
/// so there is at most one entry per target pid.
static WAITERS: IrqMutex<Vec<(&'static mut Task, u64)>> =
    IrqMutex::new_keyed(LOCK_CLASS_PARK, Vec::new());

/// Parked `ZzZ` tasks awaiting their wake deadline (absolute monotonic ns).
///
/// A sleeping task is removed from the run queue at park time and lives here
/// until `wake_sleepers` (idle loop) moves it back to `QUEUE` as `Ready`.
/// Never held together with `QUEUE`: `wake_sleepers` drops the sleep list
/// before locking the queue, and `schedule` never touches it.
///
/// Kept sorted ascending by deadline: `earliest_sleep_deadline` is an O(1)
/// front peek, and `wake_sleepers` drains a contiguous prefix with a single
/// `drain`, so a burst wakeup costs O(k) (one shift) instead of O(n·k)
/// per-element `remove` shifts.  Insertion (`sleep_until`) stays O(n), but a
/// task sleeps far less often than the idle loop scans.
static SLEEPING: IrqMutex<Vec<(u64, &'static mut Task)>> =
    IrqMutex::new_keyed(LOCK_CLASS_PARK, Vec::new());

/// Anchor context: the idle (run()/scheduler) register state. Captured by the
/// first `switch_to` away from it; restored when no ready task remains.
/// Uses `UnsafeCell` instead of `static mut` to avoid `static_mut_refs` UB
/// (MONIKA invasive fix S5). Only the BSP scheduler touches this, but the
/// compiler must not assume `&mut` aliasing across `switch_to` asm.
struct IdleCell(core::cell::UnsafeCell<Task>);
unsafe impl Sync for IdleCell {}
static IDLE: IdleCell = IdleCell(core::cell::UnsafeCell::new(Task::new(0, 0, 0, TaskContext::zeroed())));

/// Kernel page-table root. Kernel threads share it; it is also the root to
/// restore when parking an exiting task into idle.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// PIDs are 64-bit, monotonically increasing, and never reused: `NEXT_ID`
/// is only ever `fetch_add`ed — reap/GC of dead tasks never resets or recycles
/// it — so a pid uniquely identifies at most one process for the life of the
/// boot (SCHED-014). Exhausting 2^64 spawns cannot happen in practice; if it
/// ever did, we abort rather than silently reuse identities.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    let prev = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    assert!(prev < u64::MAX - 1, "next_id: pid space exhausted (would reuse)");
    prev + 1
}

/// Stash for INIT caps — installed by `load_init_from_esp` before `enter_userspace`.
/// `(caps, mirror phys, window va)`. IrqMutex for deadline-only `need_resched`.
static INIT_CAPS_STASH: IrqMutex<Option<(Arc<Vec<crate::caps::Cap>>, u64, u64)>> = IrqMutex::new(None);

pub fn stash_init_caps(caps: Arc<Vec<crate::caps::Cap>>, phys: u64, va: u64) {
    *INIT_CAPS_STASH.lock() = Some((caps, phys, va));
}
fn take_init_caps() -> Option<(Arc<Vec<crate::caps::Cap>>, u64, u64)> {
    INIT_CAPS_STASH.lock().take()
}

fn idle_ctx() -> *mut TaskContext {
    // The idle anchor task lives in a static; writing its context is inherent
    // to capturing/restoring the scheduler register state. `UnsafeCell` gives
    // a raw `*mut` without creating a `&mut` reference (S5).
    unsafe { core::ptr::addr_of_mut!((*IDLE.0.get()).ctx) }
}

/// Record the kernel page-table root. Call once from `Kernel::run` before any
/// task is spawned.
pub fn init(root: u64) {
    KERNEL_ROOT.store(root, Ordering::Relaxed);
    // Single global FIFO: mark BSP active for deadline accounting. No periodic
    // LAPIC — `sched_ticks` increments voluntarily in `schedule()` (deadline-only).
    crate::smp::set_sched_active(0, true);
}

/// The kernel page-table root shared by kernel threads and cloned for user
/// address spaces.
pub fn kernel_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// The eager user-memory table index of the task running on this CPU, or
/// `None` when in kernel context (no current task, or a kernel-only task).
pub fn current_vm() -> Option<usize> {
    let pc = crate::smp::current_per_cpu();
    let ptr = pc.current_task.load(Ordering::Relaxed);
    if ptr.is_null() {
        return None;
    }
    let t = unsafe { &*(ptr as *const Task) };
    let vm = t.vm;
    if vm == 0 { None } else { Some(vm) }
}

/// Enqueue a task for the scheduler. Returns its id.
pub fn spawn(mut task: Task) -> u64 {
    task.id = next_id();
    task.state = TaskState::Ready;
    // Durable lineage: remember parent for X→…→B walks even after parents are reaped.
    lineage_insert(task.id, task.parent_pid);
    let leaked: &'static mut Task = Box::leak(Box::new(task));
    let id = leaked.id;
    crate::unispace::provider::proc::attach(id);
    QUEUE.lock().push_back(leaked);
    preempt_kick();
    id
}

/// Enqueue a task with an explicit scheduling class. Returns the task's id.
pub fn spawn_with_priority(mut task: Task, prio: Priority) -> u64 {
    task.prio = prio;
    spawn(task)
}

// ── Slice timer (tick preemption) ──────────────────────────────────────
//
// The dispatched task's slice expiry is armed as a one-shot UniversalTimer
// entry.  UniversalTimer stays the single owner of the LAPIC one-shot, so
// sleep deadlines and slice deadlines can never fight over the arming (the
// earlier of the two simply fires first and `tick()` re-arms from the queue).
// The expiry callback only raises `need_resched`; the actual preemption
// happens in `try_preempt_from_irq`, called from the timer ISR after EOI.

static SLICE_TIMER_SEQ: AtomicU64 = AtomicU64::new(0);

fn slice_expired(_ctx: *mut u8) {
    crate::smp::set_need_resched();
}

#[inline]
fn this_cpu_timer_id(seq: u64) -> crate::services::universal_timer::TimerId {
    crate::services::universal_timer::TimerId {
        cpu: crate::smp::current_cpu_id(),
        seq,
    }
}

/// Arm (replacing any previous) the slice-expiry timer for the freshly
/// dispatched task.
fn arm_slice_timer(slice_ns: u64) {
    let old = SLICE_TIMER_SEQ.swap(0, Ordering::AcqRel);
    if old != 0 {
        crate::services::universal_timer::cancel_timer_id(this_cpu_timer_id(old));
    }
    let deadline = crate::services::universal_timer::now_ns().saturating_add(slice_ns);
    if let Some(id) =
        crate::services::universal_timer::set_oneshot(deadline, slice_expired, core::ptr::null_mut())
    {
        SLICE_TIMER_SEQ.store(id.seq, Ordering::Release);
    }
}

/// Drop to idle: no task owns a slice anymore.
fn cancel_slice_timer() {
    let old = SLICE_TIMER_SEQ.swap(0, Ordering::AcqRel);
    if old != 0 {
        crate::services::universal_timer::cancel_timer_id(this_cpu_timer_id(old));
    }
}

/// A new task became Ready: make sure a CPU-bound current gets preempted so
/// the arrival can run. If a slice timer is already armed (competition
/// existed), it will raise `need_resched` on expiry and nothing more is
/// needed; otherwise arm a one-shot kick — the LAPIC stays event-driven
/// (no periodic tick): without competition no timer runs at all (SCHED-011).
fn preempt_kick() {
    if !crate::smp::is_sched_active() {
        return;
    }
    crate::smp::set_need_resched();
    if SLICE_TIMER_SEQ.load(Ordering::Acquire) == 0 {
        arm_slice_timer(SLICE_INTERACTIVE_NS);
    }
}

/// Preemption entry from IRQ context (slice-expiry tick, any deadline tick,
/// or the resched IPI), invoked *after* EOI.
///
/// `from_user` gates the switch: only contexts interrupted in **ring 3** are
/// preempted here. Kernel-context ticks merely leave `need_resched` set,
/// consumed at the next sleep/exit/wait/idle dispatch. Rationale: large
/// parts of the kernel (VFS caches, unispace registries, drivers) still hold
/// plain spin mutexes that do not disable IRQs; switching a holder away can
/// deadlock a spinner that disables IRQs while waiting (no further ticks).
/// Until those locks are audited to IrqMutex, kernel-mode preemption stays
/// off. Ring-3 entry is safe: the CPU already switched to the task's kernel
/// stack via rsp0, and the scheduler's own locks are never held across the
/// iretq boundary.
///
/// Additional gates (both modes): `sched_active`, `preempt_count == 0`,
/// non-null current, current state == `Running`. APs never schedule.
pub fn try_preempt_from_irq(from_user: bool) {
    if !from_user {
        return;
    }
    if !crate::smp::is_sched_active() || !crate::smp::preempt_is_enabled() {
        return;
    }
    let pc = crate::smp::current_per_cpu();
    if pc.current_task.load(Ordering::Relaxed).is_null() {
        return;
    }
    // Only preempt a genuinely Running current (never mid-park/exit).
    {
        let cur = CURRENT.lock();
        match cur.as_deref() {
            Some(t) if t.state == TaskState::Running => {}
            _ => return,
        }
    }
    if !crate::smp::take_need_resched() {
        return;
    }
    crate::smp::inc_sched_ticks();
    schedule();
}

/// Fork the current user task copy-on-write (`/proc/self:fork`).
///
/// The child receives a COW clone of the caller's address space (identical
/// region bookkeeping, shared frames, writable pages downgraded in both
/// roots), its own kernel stack seeded with a byte-identical copy of the
/// caller's live `SyscallFrame` — with rax forced to 0, the fork return
/// value — and a first-entry stub that falls into the syscall-return
/// epilogue. The caller (still mid-`:fork` invoke) returns the child's pid.
///
/// Semantically: same user memory contents, same brk/stack layout, caps and
/// args inherited; fds are process-global today so nothing to duplicate.
///
/// Errors (negated errno): `-EINVAL` from a kernel-only context, `-ENOMEM`
/// when any allocation fails (all partial state is rolled back).
pub fn fork_current() -> Result<u64, i64> {
    use crate::arch::x86_64::syscall::{fork_child_resume_addr, SyscallFrame};

    let pc = crate::smp::current_per_cpu();
    let ptr = pc.current_task.load(Ordering::Relaxed);
    if ptr.is_null() {
        return Err(-22); // EINVAL: not a task context
    }
    // SAFETY: current_task points at a leaked Task for as long as it runs;
    // only immutable fields are read here.
    let parent = unsafe { &*(ptr as *const Task) };
    if parent.vm == 0 {
        return Err(-22); // EINVAL: kernel-only task has no address space to clone
    }

    let alloc = crate::mm::heap::get_phys_allocator_mut();

    // Snapshot the caller's live syscall frame. It sits directly below
    // kernel_stack_top: the entry stub builds it there immediately after the
    // rsp0 xchg, and dispatch-time RSP only ever moves further down.
    let pframe: SyscallFrame = unsafe {
        *((parent.kernel_stack_top - core::mem::size_of::<SyscallFrame>() as u64)
            as *const SyscallFrame)
    };

    // 1) COW address-space clone. The parent's supervisor caps window is
    //    excluded from the structural clone (it is re-established privately
    //    below at the child's own randomized base).
    let skip_cow = if parent.caps_phys != 0 {
        Some((parent.caps_slot_va, parent.caps_slot_va + crate::mm::layout::CAP_SLOT_SIZE))
    } else {
        None
    };
    let (vm, child_root) = crate::mm::usermem::fork_as(parent.vm, alloc, skip_cow)?;

    // 2) Child kernel stack.
    let Some((kstack_top, slot)) = alloc_kernel_stack(alloc) else {
        crate::mm::vmm::destroy_root(child_root, alloc);
        crate::mm::usermem::unregister(vm);
        return Err(-12); // ENOMEM
    };

    // 3) Seed the child frame: identical user state, rax = 0.
    let cframe = kstack_top - core::mem::size_of::<SyscallFrame>() as u64;
    unsafe {
        core::ptr::copy_nonoverlapping(&pframe as *const SyscallFrame, cframe as *mut SyscallFrame, 1);
        (*(cframe as *mut SyscallFrame)).rax = 0;
    }

    // 4) Task bookkeeping. ctx.rsp is a formality — the resume stub reloads
    //    rsp from PerCpu.syscall_rsp0 — but keep it consistent.
    let mut task = Task::new(
        kstack_top,
        child_root,
        parent.user_gs,
        TaskContext::new(kstack_top, fork_child_resume_addr()),
    );
    task.kstack_slot = slot;
    task.parent_pid = parent.id;
    task.args = String::from(parent.args.as_str());
    // Inherit PKU rights: the seeded frame returns through the syscall
    // epilogue with the parent's PKRU still in the register.
    task.pkru = parent.pkru;

    // 5) Caps: share the authoritative Arc (grant paths diverge lazily via
    //    Arc::make_mut) and give the child its own supervisor mirror page at
    //    its own randomized window base, independent of later parent grants.
    task.caps_slot_va = crate::mm::layout::pick_caps_va();
    if let Some(caps) = &parent.caps_arc {
        match install_caps(child_root, caps.as_slice(), task.caps_slot_va, alloc) {
            Some(phys) => {
                task.caps_arc = Some(caps.clone());
                task.caps_phys = phys;
                task.caps_pages = parent.caps_pages;
            }
            None => {
                free_kernel_stack(slot, alloc);
                crate::mm::vmm::destroy_root(child_root, alloc);
                crate::mm::usermem::unregister(vm);
                return Err(-12); // ENOMEM
            }
        }
    }

    Ok(spawn(task))
}

/// Park the current task until `deadline_ns` (absolute, monotonic).  The task
/// is marked `ZzZ`, registered in the sleeping list, and switched away; the
/// idle loop re-queues it as `Ready` once the deadline passes (see
/// `wake_sleepers`).  Returns only when the task is later rescheduled.
/// With no current task (kernel boot context) it returns immediately.
pub fn sleep_until(deadline_ns: u64) {
    let mut cur = CURRENT.lock();
    match cur.as_mut() {
        Some(t) => {
            let raw = &mut **t as *mut Task;
            t.state = TaskState::ZzZ;
            drop(cur);
            // Binary-insert so the list stays deadline-sorted (front = earliest).
            {
                let mut sleeping = SLEEPING.lock();
                let pos = sleeping
                    .binary_search_by(|(d, _)| d.cmp(&deadline_ns))
                    .unwrap_or_else(|e| e);
                sleeping.insert(pos, (deadline_ns, unsafe { &mut *raw }));
            }
            // The sleep lock must be dropped before `schedule`: the idle loop
            // takes `SLEEPING` again in `wake_sleepers`, so holding it across a
            // task switch would deadlock the whole scheduler.
            schedule();
        }
        None => {
            // No current task — nothing to park.
        }
    }
}

/// Park the current task for `ns` nanoseconds (relative to now).
pub fn sleep_current(ns: u64) {
    sleep_until(crate::services::universal_timer::now_ns().saturating_add(ns));
}

/// Earliest absolute deadline among sleeping tasks, if any.  Used by the idle
/// loop to arm the timer so a sleeper wakes on time.  The list is
/// deadline-sorted, so this is a front peek (O(1)).
pub fn earliest_sleep_deadline() -> Option<u64> {
    SLEEPING.lock().first().map(|(d, _)| *d)
}

/// Requeue every sleeping task whose deadline has passed.  Called from the
/// idle loop only — it may lock `QUEUE`, which must never be held across a
/// timer ISR (the ISR itself never touches scheduler locks).  `SLEEPING` is
/// dropped before `QUEUE` is taken, so the two are never held together.
pub fn wake_sleepers() {
    let now = crate::services::universal_timer::now_ns();
    let due = {
        let mut sleeping = SLEEPING.lock();
        // The list is deadline-sorted, so the expired tasks are a contiguous
        // front prefix; drain them in one shift instead of per-element removes.
        let mut n = 0;
        while n < sleeping.len() && sleeping[n].0 <= now {
            n += 1;
        }
        sleeping.drain(..n).map(|(_, t)| t).collect::<Vec<_>>()
    };
    if due.is_empty() {
        return;
    }
    let mut q = QUEUE.lock();
    for t in due {
        t.state = TaskState::Ready;
        q.push_back(t);
    }
}

/// Mark the current task Dead and switch to the next ready task.  The exit
/// code is stored on the task (retained as a zombie until a parent's `:wait`
/// consumes it or its parent dies), so a parent can collect it.  A Dead task
/// is never requeued, so this never returns.
pub fn exit_current(code: u64) -> ! {
    let pid = CURRENT.lock().as_ref().map(|t| t.id).unwrap_or(0);
    log::info!("[sched] task exit(pid={} code={})", pid, code);
    SerialPort::puts("[sched] task exit(pid=");
    SerialPort::put_u64(pid);
    SerialPort::puts(" code=");
    SerialPort::put_u64(code);
    SerialPort::puts(")\n");
    if let Some(t) = CURRENT.lock().as_mut() {
        t.exit_code = code;
    }
    kill_current()
}

/// Exit code stamped on a task that dies by `:kill` or a ring-3 fault rather
/// than a normal `exit_current`, so a waiting parent can tell them apart.
pub const KILLED_EXIT_CODE: u64 = 0xDEAD_BEEF;

/// Park the current task forever. Marks it Dead and hands the scheduler to the
/// next ready task (or idle). Never returns.
fn kill_current() -> ! {
    if let Some(t) = CURRENT.lock().as_mut() {
        t.state = TaskState::Dead;
    }
    schedule();
    loop {
        x86_64::instructions::hlt();
    }
}

/// Kill the current task after a ring-3 fault, instead of dumping the kernel.
/// The fault handler has already swapped GS to the kernel state; this marks the
/// task Dead and parks it, abandoning the handler's iretq frame — control never
/// returns to the faulting user context.
pub fn kill_user_fault() -> ! {
    let pid = CURRENT.lock().as_ref().map(|t| t.id).unwrap_or(0);
    SerialPort::puts("[sched] user fault — killing pid=");
    SerialPort::put_u64(pid);
    SerialPort::puts("\n");
    if let Some(t) = CURRENT.lock().as_mut() {
        t.exit_code = KILLED_EXIT_CODE;
    }
    kill_current()
}

/// Unified lookup across the five live sets — single place that scans
/// `CURRENT`/`QUEUE`/`SLEEPING`/`WAITERS`/`ZOMBIES` in order, never holding
/// two `IrqMutex` at once. Single global FIFO, deadline-only (no periodic
/// LAPIC). Deduplicates O(n) scans and keeps contention in one helper.
fn with_task<R>(pid: u64, mut f: impl FnMut(&Task) -> Option<R>) -> Option<R> {
    {
        let cur = CURRENT.lock();
        if let Some(t) = cur.as_ref() {
            if t.id == pid {
                if let Some(r) = f(*t) {
                    return Some(r);
                }
                // pid unique — no need to scan further, but keep same
                // semantics as before (RECLAIM not live).
                return None;
            }
        }
    }
    {
        let q = QUEUE.lock();
        for t in q.iter() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    {
        let s = SLEEPING.lock();
        for (_, t) in s.iter() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    {
        let w = WAITERS.lock();
        for (t, _) in w.iter() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    {
        let z = ZOMBIES.lock();
        for t in z.iter() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    None
}

/// Mutable twin of [`with_task`]: hands the closure `&mut Task` for `pid`,
/// scanning the five live sets one lock at a time. Exclusive access is sound
/// under the cooperative BSP-only scheduler — the task being mutated is by
/// definition not running (or is the caller itself in `CURRENT`), and no
/// other CPU can reach these lists.
fn with_task_mut<R>(pid: u64, mut f: impl FnMut(&mut Task) -> Option<R>) -> Option<R> {
    {
        let mut cur = CURRENT.lock();
        if let Some(t) = cur.as_mut() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    {
        let mut q = QUEUE.lock();
        for t in q.iter_mut() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    {
        let mut s = SLEEPING.lock();
        for (_, t) in s.iter_mut() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    {
        let mut w = WAITERS.lock();
        for (t, _) in w.iter_mut() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    // ZOMBIES stay mutable-reachable for late cap fixes; RECLAIM does not
    // (already consumed, invisible to every scan).
    {
        let mut z = ZOMBIES.lock();
        for t in z.iter_mut() {
            if t.id == pid {
                return f(*t);
            }
        }
    }
    None
}

/// Remove the live (non-zombie) task `pid` from whichever list parks it
/// (`QUEUE`/`SLEEPING`/`WAITERS`), returning it. Zombies are deliberately not
/// removable here — they are consumed only via [`take_zombie`].
fn take_from_lists(pid: u64) -> Option<&'static mut Task> {
    {
        let mut q = QUEUE.lock();
        if let Some(i) = q.iter().position(|t| t.id == pid) {
            return q.remove(i);
        }
    }
    {
        let mut s = SLEEPING.lock();
        if let Some(i) = s.iter().position(|(_, t)| t.id == pid) {
            return Some(s.remove(i).1);
        }
    }
    {
        let mut w = WAITERS.lock();
        if let Some(i) = w.iter().position(|(t, _)| t.id == pid) {
            return Some(w.remove(i).0);
        }
    }
    None
}

/// Snapshot the state of `pid` (excluding already-reaped tasks).
///
/// The scheduler is cooperative and BSP-only, so every live task is exactly one
/// of: the current task (`Running`, in `CURRENT`), a ready task in `QUEUE`, a
/// parked sleeper in `SLEEPING` (`ZzZ`), a parked waiter in `WAITERS` (`ZzZ`),
/// or a zombie in `ZOMBIES` (`Dead`). The five locks are taken and read
/// separately, never nested with each other.
pub fn process_state(pid: u64) -> Option<TaskState> {
    with_task(pid, |t| Some(t.state))
}

/// Look up the eager user-memory table index (`vm`) of `pid`, mirroring the
/// `process_state` scan of the five scheduler lists. `None` for a reaped or
/// unknown pid, or a kernel-only task (`vm == 0`).
pub fn task_vm(pid: u64) -> Option<usize> {
    with_task(pid, |t| if t.vm != 0 { Some(t.vm) } else { None })
}

/// Kill the task with id `pid`, handing it to the idle loop for reaping.
///
/// - `pid == current task`: the caller marks itself `Dead` and parks (never
///   returns), mirroring `kill_current`.
/// - a task in `QUEUE` is removed (order preserved) and parked into `ZOMBIES`;
/// - a task in `SLEEPING` is removed and parked into `ZOMBIES`;
/// - a task parked in `WAITERS` (waiting on a child) is removed and parked
///   into `ZOMBIES`; its own children then orphan out on the next reap;
/// - anything else (already reaped, or an unknown id) yields `Err(())`.
///
/// Cooperative BSP-only: every live non-executing task is in `QUEUE`,
/// `SLEEPING`, or `WAITERS`, and the only `Running` task is the caller.
pub fn kill(pid: u64) -> Result<(), ()> {
    let is_self = {
        let cur = CURRENT.lock();
        cur.as_ref().map(|t| t.id == pid).unwrap_or(false)
    };
    if is_self {
        CURRENT
            .lock()
            .as_mut()
            .map(|t| t.exit_code = KILLED_EXIT_CODE);
        kill_current(); // diverges: marked Dead and parked, never returns
    }
    if let Some(t) = take_from_lists(pid) {
        t.state = TaskState::Dead;
        t.exit_code = KILLED_EXIT_CODE;
        park_zombie(t);
        return Ok(());
    }
    Err(())
}

/// Point the current CPU's TSS.rsp0 and PerCpu.syscall_rsp0 at `top`, so
/// interrupts and syscalls land on the running task's kernel stack.
pub fn set_kernel_stack_meta(top: u64) {
    crate::arch::x86_64::gdt::set_kernel_stack(top);
    crate::smp::current_per_cpu().syscall_rsp0 = top;
}

// ── Zombies and :wait ──────────────────────────────────────────────

/// Why a `:wait` could not proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    /// The target pid does not exist (never spawned, or already reaped).
    NotFound,
    /// The target exists but is not a child of the caller.
    NotChild,
}

/// Park a just-dead task into `ZOMBIES` and requeue any task waiting on it,
/// which consumes it on the next run.
fn park_zombie(task: &'static mut Task) {
    let pid = task.id;
    ZOMBIES.lock().push(task);
    wake_waiters_for(pid);
}

/// Requeue every waiter parked on `pid` — called the moment `pid` parks into
/// `ZOMBIES`.  The waiter's `:wait` activation re-checks on resume and
/// consumes the zombie.  `WAITERS` is dropped before `QUEUE` is locked (the
/// same discipline as `wake_sleepers`).
fn wake_waiters_for(pid: u64) {
    let mut due = Vec::new();
    {
        let mut w = WAITERS.lock();
        let mut i = 0;
        while i < w.len() {
            if w[i].1 == pid {
                due.push(w.remove(i).0);
            } else {
                i += 1;
            }
        }
    }
    if due.is_empty() {
        return;
    }
    let mut q = QUEUE.lock();
    for t in due {
        t.state = TaskState::Ready;
        q.push_back(t);
    }
}

/// The recorded parent pid of `pid`, scanning every scheduler list.
fn task_parent(pid: u64) -> Option<u64> {
    with_task(pid, |t| Some(t.parent_pid))
}

/// Public accessor for `task_parent`: the recorded parent pid of `pid`, if any.
pub fn task_parent_pid(pid: u64) -> Option<u64> {
    task_parent(pid)
}

/// True if `pid` is parked in `ZOMBIES` (dead but not yet reaped).
fn zombie_present(pid: u64) -> bool {
    ZOMBIES.lock().iter().any(|t| t.id == pid)
}

/// True if `pid` is live: running, ready, sleeping, parked waiting, or zombie
/// (parked Dead retaining exit code). A zombie parent still counts as live for
/// orphan checks — its child is kept until the parent is reaped, not the moment
/// the parent parks into ZOMBIES (otherwise `kill` of a waiter could free its
/// child before the waiter consumes the exit code). RECLAIM tasks are not live
/// (already consumed, awaiting teardown). Single global, `IrqMutex` therefore
/// no ISR re-entry; deduped via `with_task`.
fn pid_live(pid: u64) -> bool {
    with_task(pid, |_| Some(())).is_some()
}

/// Remove and return the zombie `pid`, if parked.
fn take_zombie(pid: u64) -> Option<&'static mut Task> {
    let mut zombies = ZOMBIES.lock();
    let pos = zombies.iter().position(|t| t.id == pid)?;
    Some(zombies.remove(pos))
}

/// Remove `pid` from `ZOMBIES`, return its exit code, and hand the zombie to
/// `RECLAIM` for idle-loop teardown.  Teardown is deferred because
/// `destroy_root` touches the APIC (TLB-shootdown IPIs), which is only mapped
/// in the kernel root, never in a user task's root.  Once consumed, the pid is
/// gone from every scheduler scan.
fn consume_zombie(pid: u64) -> Option<u64> {
    let task = take_zombie(pid)?;
    let code = task.exit_code;
    RECLAIM.lock().push(task);
    Some(code)
}

/// Wait for `pid` (which must be a child of the caller) to exit and consume
/// its exit code.  If the target is already parked as a zombie, its exit code
/// is consumed immediately (teardown is deferred to `RECLAIM`/the idle loop);
/// otherwise parks the caller in `WAITERS` until the target exits (the exit
/// path requeues the waiter) and resumes, then re-checks.  Mirrors
/// `sleep_until`: `WAITERS` is never held across `schedule`, and no scheduler
/// list is touched from an ISR.
pub fn wait(pid: u64) -> Result<u64, WaitError> {
    let me = {
        let cur = CURRENT.lock();
        match cur.as_ref() {
            Some(t) => t.id,
            None => return Err(WaitError::NotFound),
        }
    };
    // Relationship is checked once: a task's parent never changes, and only
    // the parent may wait on a child (Unix wait() semantics, which also rules
    // out wait-cycles — a task cannot be its own parent).
    {
        let parent = task_parent(pid);
        if parent.is_none() && !zombie_present(pid) {
            return Err(WaitError::NotFound);
        }
        if parent != Some(me) {
            return Err(WaitError::NotChild);
        }
    }
    loop {
        if let Some(code) = consume_zombie(pid) {
            return Ok(code);
        }
        if !zombie_present(pid) && !pid_live(pid) {
            return Err(WaitError::NotFound);
        }
        let mut cur = CURRENT.lock();
        if let Some(t) = cur.as_mut() {
            t.state = TaskState::ZzZ;
            let raw = &mut **t as *mut Task;
            drop(cur);
            WAITERS.lock().push((unsafe { &mut *raw }, pid));
            schedule();
            continue;
        }
        return Err(WaitError::NotFound);
    }
}

/// The `:spawn` argument string recorded for `pid`, if the task still exists
/// (live or zombie).  `/proc/self/args` is backed by this.
pub fn task_args(pid: u64) -> Option<String> {
    with_task(pid, |t| Some(t.args.clone()))
}

/// The retained exit code of a zombie `pid`, if any.
pub fn task_exit_code(pid: u64) -> Option<u64> {
    ZOMBIES
        .lock()
        .iter()
        .find(|t| t.id == pid)
        .map(|t| t.exit_code)
}

/// Snapshot of `pid`'s capability set, if the task exists (live or zombie).
/// Deep-clones the task's Arc'd caps so the caller can read without holding locks.
pub fn caps_snapshot(pid: u64) -> Option<Vec<crate::caps::Cap>> {
    // Helper to clone from a Task reference.
    fn clone_from_task(t: &Task) -> Option<Vec<crate::caps::Cap>> {
        match &t.caps_arc {
            // Task with no caps: empty set for user tasks, bypass is handled by caller via None
            None => {
                if t.vm == 0 {
                    None
                } else {
                    Some(Vec::new())
                }
            }
            Some(arc) => Some((**arc).clone()),
        }
    }
    with_task(pid, clone_from_task)
}

/// Tear down one dead task: destroy its private page tables, free its kernel
/// stack, release its user-memory registration, detach its `/proc` dir, and
/// drop the task box.
///
/// Safe from any context as long as the task is not parked on its own stack —
/// the idle reaper and a consuming `:wait` both qualify (the scheduler is
/// BSP-only, so the dead root is never the active CR3).
fn reap_one(task: &'static mut Task, alloc: &mut BitmapAllocator) {
    let root = task.root;
    // Free caps window before destroying root so its leaves are still present for unmap
    if task.caps_phys != 0 {
        // If root still live, unmap the slot (destroy_root would free low-half anyway, but be explicit)
        if root != 0 && root != kernel_root() {
            free_caps_page(root, task.caps_slot_va, alloc);
        } else {
            // kernel thread caps phys still needs free (CAP_SLOT_SIZE pages)
            let pages = (crate::mm::layout::CAP_SLOT_SIZE as usize) / 4096;
            for i in 0..pages {
                unsafe { alloc.free(task.caps_phys + i as u64 * 4096) };
            }
        }
    }
    if root != 0 && root != kernel_root() {
        crate::mm::vmm::destroy_root(root, alloc);
    }
    if task.kstack_slot != usize::MAX {
        free_kernel_stack(task.kstack_slot, alloc);
    }
    if task.vm != 0 {
        // The region table alone is dropped — `destroy_root` already walked
        // the page tables and freed every user leaf frame.
        crate::mm::usermem::unregister(task.vm);
    }
    let raw = &mut *task as *mut Task;
    crate::unispace::provider::proc::detach(task.id);
    unsafe {
        drop(Box::from_raw(raw));
    }
}

/// Reclaim dead tasks, freeing their user page tables, kernel stacks, and task
/// boxes.
///
/// Called from the idle loop, which runs on the boot stack under the kernel
/// root — so the task being reaped is never the calling context, and the
/// kernel root's address space (which maps the APIC MMIO needed by
/// `destroy_root`'s TLB shootdown) is active.  Consumed zombies (exit codes
/// already collected by a parent's `:wait`) are drained first and freed
/// unconditionally; the rest are preserved while their parent is still live
/// (the parent may `:wait` them) and freed once their parent is dead.  Reading
/// the other scheduler lists while holding `ZOMBIES` is safe: nothing acquires
/// `ZOMBIES` while holding those (see `park_zombie`/`kill`/`wait`), and
/// `reap_dead` only runs in the idle loop.
///
/// After freeing stacks, TSS.rsp0 / `syscall_rsp0` are re-pinned to the top of
/// the kernel's static high `.stack` (the idle stack). Previously rsp0 was
/// left pointing at the last freed task stack until the next dispatch; that
/// was benign only because no ring-3 transition can happen while idling, but
/// it left a window where any future code path consuming rsp0 outside a task
/// switch (e.g. an early syscall/interrupt landing while "idle") would touch
/// unmapped memory. Re-pinning here makes ring-0 entry always land on live
/// memory.
pub fn reap_dead(alloc: &mut BitmapAllocator) {
    // Single global FIFO, idle-only: must be called from idle (no CURRENT)
    // and on the kernel root (APIC MMIO for TLB shootdown). APs halt (SMP
    // out) so no AP ever reaps. `IrqMutex` keeps deadline wake ISR from
    // nesting, but the idle check enforces the SCHED-L001 ordering.
    // Unconditional (not debug_assert): reaping on a user root or with a live
    // CURRENT would corrupt page tables / free a running task's state in
    // release builds too. `is_sched_active` gates the pre-`task::init` window.
    assert!(CURRENT.lock().is_none(), "reap_dead: must be idle, CURRENT Some");
    assert!(
        crate::smp::is_sched_active(),
        "reap_dead: scheduler not initialized (sched_active false)"
    );
    assert!(
        kernel_root() != 0 && crate::mm::vmm::current_root() == kernel_root(),
        "reap_dead: must be on kernel root, current={:#x} kernel={:#x}",
        crate::mm::vmm::current_root(),
        kernel_root()
    );
    {
        let mut reclaim = RECLAIM.lock();
        for task in reclaim.drain(..) {
            reap_one(task, alloc);
        }
    }
    // Zombie pass, two-phase so `pid_live` (which locks the other five
    // lists) never runs while holding `ZOMBIES` — the "no ordering cycle"
    // argument then holds structurally instead of resting on the idle-only
    // convention.
    let orphan_candidates: Vec<(usize, u64)> = {
        let zombies = ZOMBIES.lock();
        zombies
            .iter()
            .enumerate()
            // parent_pid == 0 (kernel-launched, e.g. INIT from
            // `enter_userspace`) has no possible waiter: orphaned by
            // definition, matching the pre-two-phase behavior.
            .map(|(i, t)| (i, t.parent_pid))
            .collect()
    };
    let mut free_idx: Vec<usize> = Vec::new();
    for (i, ppid) in orphan_candidates {
        if ppid == 0 || !pid_live(ppid) {
            free_idx.push(i);
        }
    }
    free_idx.sort_unstable_by(|a, b| b.cmp(a));
    let mut free: Vec<&'static mut Task> = Vec::new();
    {
        let mut zombies = ZOMBIES.lock();
        for i in free_idx {
            free.push(zombies.remove(i));
        }
    }
    for task in free {
        reap_one(task, alloc);
    }
    // All freed kstacks are gone; make ring-0 entry land on the idle stack
    // again instead of a just-freed one (see doc above).
    set_kernel_stack_meta(idle_stack_top());
    lineage_gc();
}

/// Top of the kernel's static high `.stack` — the stack the BSP idles on.
fn idle_stack_top() -> u64 {
    unsafe extern "C" {
        static __kernel_end: u8;
    }
    core::ptr::addr_of!(__kernel_end) as *const u8 as u64
}

/// Weighted round-robin budget: Interactive dispatches remaining before one
/// Batch dispatch resets it.
static WRR_CREDIT: AtomicU32 = AtomicU32::new(WEIGHT_INTERACTIVE);

/// Preemptive scheduler: switch to the next ready task.
///
/// The outgoing task is requeued if it is still `Running` (preempted with its
/// slice exhausted or transitioning); a `Dead` task (from `exit_current`) is
/// parked and the scheduler drops to idle. No spin lock is held across
/// `switch_to`.
///
/// Single global queue, weighted round-robin: Interactive tasks are picked
/// while their 3:1 budget allows, else Batch (`pick` below). The slice-expiry
/// timer is armed only while competition remains in the queue; a lone task
/// runs untimed and arrivals re-arm via `preempt_kick` (spawn/fork). Entry
/// from IRQ context goes through `try_preempt_from_irq`
/// (SCHED-L001 keeps tick-during-lock impossible).
pub fn schedule() {
    debug_assert!(
        crate::smp::is_sched_active(),
        "schedule: called before task::init"
    );
    crate::smp::preempt_disable();
    // Deadline-only: `need_resched` may be set by deadline expiry helpers
    // (never a periodic LAPIC tick). Consume here.
    let _ = crate::smp::take_need_resched();
    crate::smp::inc_sched_ticks();
    // Wake expired sleepers before picking the next task so a burst of
    // sleepers never starves. This was
    // previously only in the idle loops (Kernel::run / enter_userspace) which
    // starved sleepers under load (S1).
    wake_sleepers();

    let prev = CURRENT.lock().take();
    let mut q = QUEUE.lock();

    // Weighted round-robin pick: first Ready Interactive and first Ready
    // Batch; prefer Interactive while its budget remains, else Batch. O(n)
    // scan — the queue is small; per-CPU queues will replace this later.
    let mut inter: Option<usize> = None;
    let mut batch: Option<usize> = None;
    for i in 0..q.len() {
        if q[i].state != TaskState::Ready {
            continue;
        }
        match q[i].prio {
            Priority::Interactive if inter.is_none() => inter = Some(i),
            Priority::Batch if batch.is_none() => batch = Some(i),
            _ => {}
        }
    }
    let next = loop {
        if let Some(i) = inter {
            let c = WRR_CREDIT.load(Ordering::Relaxed);
            if c > 0 {
                WRR_CREDIT.store(c - 1, Ordering::Relaxed);
                break q.remove(i);
            }
        }
        if let Some(i) = batch {
            WRR_CREDIT.store(WEIGHT_INTERACTIVE, Ordering::Relaxed);
            break q.remove(i);
        }
        // Only Interactive ready (or nothing): spend it regardless of budget.
        if let Some(i) = inter {
            let c = WRR_CREDIT.load(Ordering::Relaxed);
            WRR_CREDIT.store(c.saturating_sub(1), Ordering::Relaxed);
            break q.remove(i);
        }
        break None;
    };

    let (next_ptr, next_root) = match next {
        Some(t) => {
            t.state = TaskState::Running;
            // Arm the expiry timer only while competition remains in the
            // queue; a lone task runs untimed (no periodic tick, SCHED-011).
            // A later arrival re-arms via `preempt_kick` (spawn/fork).
            let more = q.iter().any(|t| t.state == TaskState::Ready);
            let slice = t.prio.slice_ns();
            let root = t.root;
            let stack_top = t.kernel_stack_top;
            let ctx_ptr = core::ptr::addr_of_mut!(t.ctx);
            set_kernel_stack_meta(stack_top);
            #[cfg(target_arch = "x86_64")]
            crate::arch::x86_64::cpufeat::pku_apply(t.pkru);
            crate::smp::current_per_cpu().current_task.store(t as *mut Task as *mut core::ffi::c_void, Ordering::Relaxed);
            *CURRENT.lock() = Some(t);
            if more {
                arm_slice_timer(slice);
            } else {
                cancel_slice_timer();
            }
            (ctx_ptr, root)
        }
        None => {
            // No ready task.
            match prev {
                Some(p) if p.state == TaskState::Dead || p.state == TaskState::ZzZ => {
                    // Park the current task and resume idle.  A Dead task is
                    // parked into ZOMBIES (after `drop(q)`, so ZOMBIES is
                    // never acquired while QUEUE is held) for a later idle
                    // loop reap or a parent's :wait; a ZzZ task is already
                    // registered in the sleeping/waiter list, so it is simply
                    // left out of the queue.  `pctx` stays valid either way.
                    crate::smp::current_per_cpu().current_task.store(core::ptr::null_mut(), Ordering::Relaxed);
                    *CURRENT.lock() = None;
                    cancel_slice_timer();
                    let pctx = core::ptr::addr_of_mut!(p.ctx);
                    let root = KERNEL_ROOT.load(Ordering::Relaxed);
                    drop(q);
                    if p.state == TaskState::Dead {
                        park_zombie(p);
                    }
                    crate::mm::vmm::set_current_root(root);
                    // This context (a Dead task) never resumes through here,
                    // so the preempt-disable taken on entry MUST be released
                    // before the final switch — post-switch release would
                    // leak a per-CPU disable.
                    crate::smp::preempt_enable();
                    unsafe {
                        switch_to(pctx, idle_ctx(), root);
                    }
                    return;
                }
                Some(p) => {
                    // Empty-queue fast-path: keep running. Previously
                    // this requeued into QUEUE and left CURRENT=None /
                    // PerCpu.current_task=null while the task continued on its
                    // stack (S0). Now restore CURRENT/PerCpu and keep Running.
                    let stack_top = p.kernel_stack_top;
                    let ptr = &mut *p as *mut Task as *mut core::ffi::c_void;
                    // Keep state Running (p was Running before take).
                    debug_assert!(p.state == TaskState::Running);
                    // Alone on the CPU: run untimed. No slice-expiry timer is
                    // armed, so a lone CPU-bound task is not interrupted
                    // periodically; the next arrival re-arms via
                    // `preempt_kick` (spawn/fork) or a sleep deadline tick
                    // (wake_sleepers → Ready + need_resched).
                    crate::smp::current_per_cpu().current_task.store(ptr, Ordering::Relaxed);
                    *CURRENT.lock() = Some(p);
                    set_kernel_stack_meta(stack_top);
                    cancel_slice_timer();
                    drop(q);
                    crate::smp::preempt_enable();
                    return;
                }
                None => {
                    crate::smp::current_per_cpu().current_task.store(core::ptr::null_mut(), Ordering::Relaxed);
                    *CURRENT.lock() = None;
                    cancel_slice_timer();
                    drop(q);
                    crate::smp::preempt_enable();
                    return;
                }
            }
        }
    };

    match prev {
        // A Dead or ZzZ task may also switch straight to the next ready task
        // (queue non-empty).  Only a Dead task is parked into ZOMBIES — a
        // ZzZ task is already registered in the sleeping/waiter list — both
        // dealt with after `drop(q)`.
        Some(p) if p.state == TaskState::Dead || p.state == TaskState::ZzZ => {
            let pctx = core::ptr::addr_of_mut!(p.ctx);
            drop(q);
            if p.state == TaskState::Dead {
                park_zombie(p);
            }
            crate::mm::vmm::set_current_root(next_root);
            // A Dead task never resumes through here (ZzZ resumes via its
            // registered ctx on wake, but releasing before the switch is the
            // one placement balanced for both cases).
            crate::smp::preempt_enable();
            unsafe {
                switch_to(pctx, next_ptr, next_root);
            }
        }
        Some(p) => {
            let pctx = core::ptr::addr_of_mut!(p.ctx);
            p.state = TaskState::Ready;
            q.push_back(p);
            drop(q);
            crate::mm::vmm::set_current_root(next_root);
            unsafe {
                switch_to(pctx, next_ptr, next_root);
            }
            crate::smp::preempt_enable();
        }
        None => {
            drop(q);
            crate::mm::vmm::set_current_root(next_root);
            unsafe {
                switch_to(idle_ctx(), next_ptr, next_root);
            }
            crate::smp::preempt_enable();
        }
    }
}

/// Launch a task directly into ring 3 (used once a loader has built a user
/// address space). Builds an iretq frame on the task's kernel stack, programs
/// the kernel/user GS pair, then switches away from idle into the new task.
///
/// Returns only when the launched task has exited and been parked back into
/// idle (the resumed caller then owns the idle loop), at which point it
/// returns the task's pid — not yet reaped, so its `/proc` dir still exists.
/// A live task never returns through this function — it runs until
/// `exit_current` parks it.
pub fn enter_userspace(
    entry: u64,
    user_stack_top: u64,
    root: u64,
    user_gs: u64,
    vm: usize,
    alloc: &mut BitmapAllocator,
) -> u64 {
    // This task gets its own slot in the fixed kernel-stack window; the iretq
    // frame lives on top of it.
    let (kernel_stack_top, slot) =
        alloc_kernel_stack(alloc).expect("enter_userspace: no kernel stack slot");
    // 5-word iretq frame at the top of the kernel stack (RIP, CS, RFLAGS,
    // RSP, SS) — `user_iret` pops exactly this.
    let frame_base = kernel_stack_top - 40;
    unsafe {
        *(frame_base as *mut u64) = entry; // RIP
        *(frame_base as *mut u64).add(1) = 0x2B; // user CS
        *(frame_base as *mut u64).add(2) = 0x202; // RFLAGS: IF set
        *(frame_base as *mut u64).add(3) = user_stack_top;
        *(frame_base as *mut u64).add(4) = 0x23; // user SS
    }

    // Kernel GS pair: GS.base = PerCpu, KERNEL_GS_BASE = user GS. `user_iret`
    // performs the final swapgs before iretq, so the user lands with
    // GS.base = user GS / KERNEL_GS_BASE = PerCpu.
    crate::arch::x86_64::syscall::set_user_gs(user_gs);

    let mut task = Task::new(
        kernel_stack_top,
        root,
        user_gs,
        TaskContext::new(frame_base, user_iret_addr()),
    );
    task.kstack_slot = slot;
    task.vm = vm;
    // Adopt stashed INIT caps if present (load_init_from_esp installed them on this root)
    if let Some((caps, phys, va)) = take_init_caps() {
        task.caps_arc = Some(caps);
        task.caps_phys = phys;
        task.caps_slot_va = va;
    }
    task.id = next_id();
    lineage_insert(task.id, task.parent_pid);
    crate::unispace::provider::proc::attach(task.id);
    task.state = TaskState::Running;
    let t: &'static mut Task = Box::leak(Box::new(task));
    let pid = t.id;
    let ctx_ptr = core::ptr::addr_of_mut!(t.ctx);
    set_kernel_stack_meta(kernel_stack_top);
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::cpufeat::pku_apply(t.pkru);
    crate::smp::current_per_cpu().current_task.store(t as *mut Task as *mut core::ffi::c_void, Ordering::Relaxed);
    *CURRENT.lock() = Some(t);

    crate::mm::vmm::set_current_root(root);
    unsafe {
        switch_to(idle_ctx(), ctx_ptr, root);
    }
    // The scheduler reaches idle not only when the launched task exits, but
    // whenever no task is ready — including while it is parked in a blocking
    // syscall (e.g. `:wait` on a child it spawned). Keep idling until the
    // launched task is actually Dead (parked in ZOMBIES) or reaped, so the
    // caller — which reads back its `/proc` — never acts on half-written
    // output. Its exit code is not consumed here; the caller drains its
    // `/proc/<pid>/std/out` before any idle reaper detaches the dir.
    loop {
        match process_state(pid) {
            Some(TaskState::Dead) | None => break,
            _ => {
                // Requeue sleepers whose deadline has passed.  `schedule()`
                // already wakes, but keep this wake for the case where
                // `schedule` found no Ready task and returned without
                // switching (fast-path keeps Running, so wake inside
                // schedule covers the rest).
                wake_sleepers();
                schedule();
                // Park until the earliest sleeping deadline so the loop
                // doesn't hot-spin while only sleepers remain. When no
                // sleeper exists we must still halt until the next IRQ
                // (e.g. INIT parked in WAITERS, audio pump sleeping) — the
                // old loop spun at 100% here (S3).
                if let Some(d) = earliest_sleep_deadline() {
                    crate::services::universal_timer::wait_until(d.saturating_add(1));
                } else {
                    crate::arch::CurrentArch::halt();
                }
            }
        }
    }
    pid
}

// ── Boot smoke test ────────────────────────────────────────────────
//
// Two kernel-only tasks alternate on serial, proving the context switch works
// before any user mode exists. Runs once at boot and exits into idle (and
// gets reaped from the idle loop). Gated behind the `selftest` feature.

#[cfg(feature = "selftest")]
const SMOKE_ITERS: u32 = 5;

/// Explicit ABI-stable entry point for the first smoke task.  These must not
/// be local closures: a closure coerced to `fn()` enters through a compiler
/// generated `FnOnce` shim, but `switch_to` starts execution from a fabricated
/// context rather than from a normal call frame.
#[cfg(feature = "selftest")]
extern "C" fn smoke_task_a() -> ! {
    for _ in 0..SMOKE_ITERS {
        SerialPort::puts("[task] A\n");
        schedule();
    }
    exit_current(0)
}

/// Explicit ABI-stable entry point for the second smoke task.
#[cfg(feature = "selftest")]
extern "C" fn smoke_task_b() -> ! {
    for _ in 0..SMOKE_ITERS {
        SerialPort::puts("[task] B\n");
        schedule();
    }
    exit_current(1)
}

/// Snapshot for unispace: (next_id, qlen, current_present, sleeping, waiters, zombies, reclaim, kstack_in_use)
pub fn sched_snapshot() -> (u64, usize, bool, usize, usize, usize, usize, usize) {
    let next = NEXT_ID.load(Ordering::Relaxed);
    let qlen = QUEUE.lock().len();
    let cur = CURRENT.lock().is_some();
    let sleeping = SLEEPING.lock().len();
    let waiters = WAITERS.lock().len();
    let zombies = ZOMBIES.lock().len();
    let reclaim = RECLAIM.lock().len();
    let kstack_used = KSTACK_IN_USE.lock().iter().filter(|&&b| b).count();
    (next, qlen, cur, sleeping, waiters, zombies, reclaim, kstack_used)
}

pub fn kstack_snapshot() -> alloc::vec::Vec<(usize, u64)> {
    let in_use = KSTACK_IN_USE.lock();
    let mut out = alloc::vec::Vec::new();
    for (slot, &used) in in_use.iter().enumerate() {
        if used {
            let base = KSTACK_VADDR_BASE - (slot as u64) * KSTACK_SIZE;
            out.push((slot, base));
        }
    }
    out
}

pub fn queue_snapshot() -> alloc::vec::Vec<(u64, u8)> {
    let q = QUEUE.lock();
    q.iter().map(|t| (t.id, t.state as u8)).collect()
}

pub fn lineage_snapshot() -> alloc::vec::Vec<(u64, u64)> {
    lineage_map().lock().iter().map(|(&k, &v)| (k, v)).collect()
}

/// Spawn two kernel-only tasks that alternate on serial, then run the
/// scheduler. Returns to the caller (idle) once both tasks have exited.
#[cfg(feature = "selftest")]
pub fn smoke_test(alloc: &mut BitmapAllocator) {
    let root = KERNEL_ROOT.load(Ordering::Relaxed);

    // Each smoke task runs on its own slot in the fixed kernel-stack window
    // (uniform with every other task, and reaped with it once parked).
    let (top_a, slot_a) = alloc_kernel_stack(alloc).expect("smoke: kernel stack slots exhausted");
    let (top_b, slot_b) = alloc_kernel_stack(alloc).expect("smoke: kernel stack slots exhausted");
    // Entry RSP must be 8 mod 16 (SysV callee entry) — top minus 8.
    let mut ta = Task::new(
        top_a,
        root,
        0,
        TaskContext::new(top_a - 8, smoke_task_a as *const () as usize as u64),
    );
    ta.kstack_slot = slot_a;
    let mut tb = Task::new(
        top_b,
        root,
        0,
        TaskContext::new(top_b - 8, smoke_task_b as *const () as usize as u64),
    );
    tb.kstack_slot = slot_b;
    spawn(ta);
    spawn(tb);

    SerialPort::puts("[task] smoke test starting\n");
    schedule();
    SerialPort::puts("[task] smoke test done\n");
}
