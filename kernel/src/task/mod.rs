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
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::drivers::serial::SerialPort;
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

/// A schedulable unit of execution.
pub struct Task {
    pub id: u64,
    pub state: TaskState,
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
}

impl Task {
    pub const fn new(kernel_stack_top: u64, root: u64, user_gs: u64, ctx: TaskContext) -> Self {
        Task {
            id: 0,
            state: TaskState::Ready,
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
        }
    }
}

// ── Durable lineage tracking ────────────────────────────────────────────
// Keep pid→parent for every spawn, even after Task is reaped, so that
// `B→A→Z→Y→X` can be walked through dead `A`. GC removes entries that are
// no longer ancestors of any live Task.
static LINEAGE: Once<Mutex<HashMap<u64, u64>>> = Once::new();
fn lineage_map() -> &'static Mutex<HashMap<u64, u64>> {
    LINEAGE.call_once(|| Mutex::new(HashMap::new()))
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

/// Liveness bitmap for the fixed task-stack window slots.
static KSTACK_IN_USE: Mutex<[bool; MAX_KSTACKS]> = Mutex::new([false; MAX_KSTACKS]);

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

/// Allocate and map a capability supervisor window for `root` at `CAP_SLOT_VA`
/// (supervisor-only READ, no USER). Returns the base physical frame (2 contiguous 4K).
/// The caller fills it via `caps::serialize_to_page`.
pub fn alloc_caps_page(root: u64, alloc: &mut BitmapAllocator) -> Option<u64> {
    let phys = alloc.alloc_contiguous(2)?;
    let va = crate::mm::layout::CAP_SLOT_VA;
    unsafe {
        core::ptr::write_bytes(crate::mm::layout::to_physmap(phys) as *mut u8, 0, 4096);
        core::ptr::write_bytes(crate::mm::layout::to_physmap(phys + 4096) as *mut u8, 0, 4096);
    }
    let mut vmm = Vmm::from_root(root);
    vmm.map_4k(alloc, va, phys, PageFlags::READ);
    vmm.map_4k(alloc, va + 4096, phys + 4096, PageFlags::READ);
    Some(phys)
}

/// Unmap and free the capability supervisor window for `root`.
pub fn free_caps_page(root: u64, alloc: &mut BitmapAllocator) {
    let va = crate::mm::layout::CAP_SLOT_VA;
    let mut vmm = Vmm::from_root(root);
    let mut frames: Vec<u64> = Vec::new();
    vmm.unmap_range_collect(alloc, va, crate::mm::layout::CAP_SLOT_SIZE, &mut frames);
    debug_assert!(frames.len() <= 2, "caps window should be at most 2 pages");
    for p in frames {
        unsafe { alloc.free(p); }
    }
}

/// Serialize caps to the supervisor mirror page and map it into `root`.
/// Returns the physical frame (2 pages, see `alloc_caps_page`). The caller
/// owns the authoritative set as an `Arc<Vec<Cap>>` on the Task.
pub fn install_caps(root: u64, caps: &[crate::caps::Cap], alloc: &mut BitmapAllocator) -> Option<u64> {
    let phys = alloc_caps_page(root, alloc)?;
    crate::caps::serialize_to_page(caps, phys);
    Some(phys)
}

/// Grant a capability to the task with `pid`, allocating a supervisor page if needed.
/// Returns `Ok` if granted or already present, `Err` if OOM or invalid.
pub fn grant_cap_to_pid(pid: u64, cap: crate::caps::Cap) -> Result<(), crate::unispace::UnispaceError> {
    crate::caps::validate_cap(&cap)?;
    // Helper to try to grant to a &mut Task
    fn do_grant(task: &mut Task, cap: crate::caps::Cap) -> Result<(), crate::unispace::UnispaceError> {
        if task.caps_arc.is_none() {
            let alloc = crate::mm::heap::get_phys_allocator_mut();
            let new_caps = Vec::new();
            if let Some(phys) = install_caps(task.root, &new_caps, alloc) {
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
        caps.push(cap);
        if task.caps_phys != 0 {
            crate::caps::serialize_to_page(caps, task.caps_phys);
        }
        Ok(())
    }

    // Search CURRENT
    {
        let mut cur = CURRENT.lock();
        if let Some(t) = cur.as_mut() {
            if t.id == pid {
                return do_grant(*t, cap);
            }
        }
    }
    // QUEUE
    {
        let mut q = QUEUE.lock();
        for t in q.iter_mut() {
            if t.id == pid {
                return do_grant(*t, cap);
            }
        }
    }
    // SLEEPING
    {
        let mut s = SLEEPING.lock();
        for (_, t) in s.iter_mut() {
            if t.id == pid {
                return do_grant(*t, cap);
            }
        }
    }
    // WAITERS
    {
        let mut w = WAITERS.lock();
        for (t, _) in w.iter_mut() {
            if t.id == pid {
                return do_grant(*t, cap);
            }
        }
    }
    // ZOMBIES
    {
        let mut z = ZOMBIES.lock();
        for t in z.iter_mut() {
            if t.id == pid {
                return do_grant(*t, cap);
            }
        }
    }
    Err(crate::unispace::UnispaceError::NotFound)
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

/// Global FIFO of runnable tasks.
static QUEUE: Mutex<VecDeque<&'static mut Task>> = Mutex::new(VecDeque::new());

/// The task currently running on this CPU, or `None` when idle.
static CURRENT: Mutex<Option<&'static mut Task>> = Mutex::new(None);

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
/// there is no ordering cycle.
static ZOMBIES: Mutex<Vec<&'static mut Task>> = Mutex::new(Vec::new());

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
static RECLAIM: Mutex<Vec<&'static mut Task>> = Mutex::new(Vec::new());

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
static WAITERS: Mutex<Vec<(&'static mut Task, u64)>> = Mutex::new(Vec::new());

/// Parked `ZzZ` tasks awaiting their wake deadline (absolute monotonic ns).
///
/// A sleeping task is removed from the run queue at park time and lives here
/// until `wake_sleepers` (idle loop) moves it back to `QUEUE` as `Ready`.
/// Never held together with `QUEUE`: `wake_sleepers` drops the sleep list
/// before locking the queue, and `schedule`/`yield` never touch it.
///
/// Kept sorted ascending by deadline: `earliest_sleep_deadline` is an O(1)
/// front peek, and `wake_sleepers` drains a contiguous prefix with a single
/// `drain`, so a burst wakeup costs O(k) (one shift) instead of O(n·k)
/// per-element `remove` shifts.  Insertion (`sleep_until`) stays O(n), but a
/// task sleeps far less often than the idle loop scans.
static SLEEPING: Mutex<Vec<(u64, &'static mut Task)>> = Mutex::new(Vec::new());

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

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Stash for INIT caps — installed by `load_init_from_esp` before `enter_userspace`.
static INIT_CAPS_STASH: Mutex<Option<(Arc<Vec<crate::caps::Cap>>, u64)>> = Mutex::new(None);

pub fn stash_init_caps(caps: Arc<Vec<crate::caps::Cap>>, phys: u64) {
    *INIT_CAPS_STASH.lock() = Some((caps, phys));
}
fn take_init_caps() -> Option<(Arc<Vec<crate::caps::Cap>>, u64)> {
    INIT_CAPS_STASH.lock().take()
}

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1
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
    id
}

/// Cooperative yield: move the current task to the tail of the run queue and
/// run the next ready task, if any.
pub fn yield_now() {
    schedule();
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

/// Snapshot the state of `pid` (excluding already-reaped tasks).
///
/// The scheduler is cooperative and BSP-only, so every live task is exactly one
/// of: the current task (`Running`, in `CURRENT`), a ready task in `QUEUE`, a
/// parked sleeper in `SLEEPING` (`ZzZ`), a parked waiter in `WAITERS` (`ZzZ`),
/// or a zombie in `ZOMBIES` (`Dead`). The five locks are taken and read
/// separately, never nested with each other.
pub fn process_state(pid: u64) -> Option<TaskState> {
    {
        let cur = CURRENT.lock();
        if let Some(t) = cur.as_ref() {
            if t.id == pid {
                return Some(t.state);
            }
        }
    }
    {
        let q = QUEUE.lock();
        for t in q.iter() {
            if t.id == pid {
                return Some(t.state);
            }
        }
    }
    {
        let s = SLEEPING.lock();
        for (_, t) in s.iter() {
            if t.id == pid {
                return Some(t.state);
            }
        }
    }
    {
        let w = WAITERS.lock();
        for (t, _) in w.iter() {
            if t.id == pid {
                return Some(TaskState::ZzZ);
            }
        }
    }
    {
        let z = ZOMBIES.lock();
        for t in z.iter() {
            if t.id == pid {
                return Some(TaskState::Dead);
            }
        }
    }
    None
}

/// Look up the eager user-memory table index (`vm`) of `pid`, mirroring the
/// `process_state` scan of the five scheduler lists. `None` for a reaped or
/// unknown pid, or a kernel-only task (`vm == 0`).
pub fn task_vm(pid: u64) -> Option<usize> {
    {
        let cur = CURRENT.lock();
        if let Some(t) = cur.as_ref() {
            if t.id == pid && t.vm != 0 {
                return Some(t.vm);
            }
        }
    }
    {
        let q = QUEUE.lock();
        for t in q.iter() {
            if t.id == pid && t.vm != 0 {
                return Some(t.vm);
            }
        }
    }
    {
        let s = SLEEPING.lock();
        for (_, t) in s.iter() {
            if t.id == pid && t.vm != 0 {
                return Some(t.vm);
            }
        }
    }
    {
        let w = WAITERS.lock();
        for (t, _) in w.iter() {
            if t.id == pid && t.vm != 0 {
                return Some(t.vm);
            }
        }
    }
    {
        let z = ZOMBIES.lock();
        for t in z.iter() {
            if t.id == pid && t.vm != 0 {
                return Some(t.vm);
            }
        }
    }
    None
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
    {
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
    }
    {
        let mut q = QUEUE.lock();
        let mut i = 0;
        while i < q.len() {
            if q[i].id == pid {
                if let Some(t) = q.remove(i) {
                    t.state = TaskState::Dead;
                    t.exit_code = KILLED_EXIT_CODE;
                    drop(q); // ZOMBIES is never acquired while QUEUE is held
                    park_zombie(t);
                    return Ok(());
                }
            }
            i += 1;
        }
    }
    {
        let mut s = SLEEPING.lock();
        for i in 0..s.len() {
            if s[i].1.id == pid {
                let t = s.remove(i).1;
                t.state = TaskState::Dead;
                t.exit_code = KILLED_EXIT_CODE;
                drop(s);
                park_zombie(t);
                return Ok(());
            }
        }
    }
    {
        let mut w = WAITERS.lock();
        for i in 0..w.len() {
            if w[i].0.id == pid {
                let t = w.remove(i).0;
                t.state = TaskState::Dead;
                t.exit_code = KILLED_EXIT_CODE;
                drop(w);
                park_zombie(t);
                return Ok(());
            }
        }
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
    if let Some(t) = CURRENT.lock().as_ref() {
        if t.id == pid {
            return Some(t.parent_pid);
        }
    }
    {
        let q = QUEUE.lock();
        for t in q.iter() {
            if t.id == pid {
                return Some(t.parent_pid);
            }
        }
    }
    {
        let s = SLEEPING.lock();
        for (_, t) in s.iter() {
            if t.id == pid {
                return Some(t.parent_pid);
            }
        }
    }
    {
        let w = WAITERS.lock();
        for (t, _) in w.iter() {
            if t.id == pid {
                return Some(t.parent_pid);
            }
        }
    }
    {
        let z = ZOMBIES.lock();
        for t in z.iter() {
            if t.id == pid {
                return Some(t.parent_pid);
            }
        }
    }
    None
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
/// (already consumed, awaiting teardown).
fn pid_live(pid: u64) -> bool {
    if let Some(t) = CURRENT.lock().as_ref() {
        if t.id == pid {
            return true;
        }
    }
    if QUEUE.lock().iter().any(|t| t.id == pid) {
        return true;
    }
    if SLEEPING.lock().iter().any(|(_, t)| t.id == pid) {
        return true;
    }
    if WAITERS.lock().iter().any(|(t, _)| t.id == pid) {
        return true;
    }
    if ZOMBIES.lock().iter().any(|t| t.id == pid) {
        return true;
    }
    false
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
    if let Some(t) = CURRENT.lock().as_ref() {
        if t.id == pid {
            return Some(t.args.clone());
        }
    }
    {
        let q = QUEUE.lock();
        for t in q.iter() {
            if t.id == pid {
                return Some(t.args.clone());
            }
        }
    }
    {
        let s = SLEEPING.lock();
        for (_, t) in s.iter() {
            if t.id == pid {
                return Some(t.args.clone());
            }
        }
    }
    {
        let w = WAITERS.lock();
        for (t, _) in w.iter() {
            if t.id == pid {
                return Some(t.args.clone());
            }
        }
    }
    {
        let z = ZOMBIES.lock();
        for t in z.iter() {
            if t.id == pid {
                return Some(t.args.clone());
            }
        }
    }
    None
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
    if let Some(t) = CURRENT.lock().as_ref() {
        if t.id == pid {
            return clone_from_task(t);
        }
    }
    {
        let q = QUEUE.lock();
        for t in q.iter() {
            if t.id == pid {
                return clone_from_task(t);
            }
        }
    }
    {
        let s = SLEEPING.lock();
        for (_, t) in s.iter() {
            if t.id == pid {
                return clone_from_task(t);
            }
        }
    }
    {
        let w = WAITERS.lock();
        for (t, _) in w.iter() {
            if t.id == pid {
                return clone_from_task(t);
            }
        }
    }
    {
        let z = ZOMBIES.lock();
        for t in z.iter() {
            if t.id == pid {
                return clone_from_task(t);
            }
        }
    }
    None
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
            free_caps_page(root, alloc);
        } else {
            // kernel thread caps phys still needs free (2 pages)
            unsafe {
                alloc.free(task.caps_phys);
                alloc.free(task.caps_phys + 4096);
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
/// TSS.rsp0 may still point at a freed stack after this; that is safe because
/// rsp0 is only consumed on a ring-3→ring-0 transition, and no user task runs
/// while the BSP is idling.  The next task switch re-pins rsp0.
pub fn reap_dead(alloc: &mut BitmapAllocator) {
    {
        let mut reclaim = RECLAIM.lock();
        for task in reclaim.drain(..) {
            reap_one(task, alloc);
        }
    }
    let mut free: Vec<&'static mut Task> = Vec::new();
    {
        let mut zombies = ZOMBIES.lock();
        let mut i = 0;
        while i < zombies.len() {
            if zombies[i].parent_pid == 0 || !pid_live(zombies[i].parent_pid) {
                free.push(zombies.remove(i));
            } else {
                i += 1;
            }
        }
    }
    for task in free {
        reap_one(task, alloc);
    }
    lineage_gc();
}

/// Cooperative scheduler: switch to the next ready task.
///
/// The outgoing task is requeued only if it is still `Running` (a voluntary
/// yield). A `Dead` task (from `exit_current`) is parked and the scheduler
/// drops to idle. No spin lock is held across `switch_to`.
///
/// BSP-only cooperative: interrupts remain enabled, but `schedule` itself is
/// not re-entered from IRQ. Future preemptive wiring must wrap with
/// `preempt_disable`/`preempt_enable` and use `IrqSafeLock`.
pub fn schedule() {
    // Cooperative BSP-only: timer ISR never touches scheduler locks (S9).
    // If preemptive tick is ever wired, this must be wrapped with
    // `preempt_disable`/`preempt_enable` and locks switched to `IrqSafeLock`.
    // For now assert we are not in IRQ context (interrupts may be enabled,
    // but we are not re-entered).
    debug_assert!(crate::smp::preempt_is_enabled(), "schedule: preempt disabled nested?");
    // Wake expired sleepers before picking the next task so a burst of
    // sleepers never starves while Ready tasks continuously yield. This was
    // previously only in the idle loops (Kernel::run / enter_userspace) which
    // starved sleepers under load (S1).
    wake_sleepers();

    let prev = CURRENT.lock().take();
    let mut q = QUEUE.lock();

    // Pop the first ready task (FIFO round-robin).
    let mut next = None;
    for i in 0..q.len() {
        if q[i].state == TaskState::Ready {
            next = q.remove(i);
            break;
        }
    }

    let (next_ptr, next_root) = match next {
        Some(t) => {
            t.state = TaskState::Running;
            let root = t.root;
            let stack_top = t.kernel_stack_top;
            let ctx_ptr = core::ptr::addr_of_mut!(t.ctx);
            set_kernel_stack_meta(stack_top);
            crate::smp::current_per_cpu().current_task.store(t as *mut Task as *mut core::ffi::c_void, Ordering::Relaxed);
            *CURRENT.lock() = Some(t);
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
                    let pctx = core::ptr::addr_of_mut!(p.ctx);
                    let root = KERNEL_ROOT.load(Ordering::Relaxed);
                    drop(q);
                    if p.state == TaskState::Dead {
                        park_zombie(p);
                    }
                    unsafe {
                        switch_to(pctx, idle_ctx(), root);
                    }
                    return;
                }
                Some(p) => {
                    // Self-yield with an empty queue: keep running. Previously
                    // this requeued into QUEUE and left CURRENT=None /
                    // PerCpu.current_task=null while the task continued on its
                    // stack (S0). Now restore CURRENT/PerCpu and keep Running.
                    let stack_top = p.kernel_stack_top;
                    let ptr = &mut *p as *mut Task as *mut core::ffi::c_void;
                    // Keep state Running (p was Running before take).
                    debug_assert!(p.state == TaskState::Running);
                    crate::smp::current_per_cpu().current_task.store(ptr, Ordering::Relaxed);
                    *CURRENT.lock() = Some(p);
                    set_kernel_stack_meta(stack_top);
                    drop(q);
                    return;
                }
                None => {
                    crate::smp::current_per_cpu().current_task.store(core::ptr::null_mut(), Ordering::Relaxed);
                    *CURRENT.lock() = None;
                    drop(q);
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
            unsafe {
                switch_to(pctx, next_ptr, next_root);
            }
        }
        Some(p) => {
            let pctx = core::ptr::addr_of_mut!(p.ctx);
            p.state = TaskState::Ready;
            q.push_back(p);
            drop(q);
            unsafe {
                switch_to(pctx, next_ptr, next_root);
            }
        }
        None => {
            drop(q);
            unsafe {
                switch_to(idle_ctx(), next_ptr, next_root);
            }
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
    if let Some((caps, phys)) = take_init_caps() {
        task.caps_arc = Some(caps);
        task.caps_phys = phys;
    }
    task.id = next_id();
    lineage_insert(task.id, task.parent_pid);
    crate::unispace::provider::proc::attach(task.id);
    task.state = TaskState::Running;
    let t: &'static mut Task = Box::leak(Box::new(task));
    let pid = t.id;
    let ctx_ptr = core::ptr::addr_of_mut!(t.ctx);
    set_kernel_stack_meta(kernel_stack_top);
    crate::smp::current_per_cpu().current_task.store(t as *mut Task as *mut core::ffi::c_void, Ordering::Relaxed);
    *CURRENT.lock() = Some(t);

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
                // `schedule` found no Ready and returned without switching
                // (self-yield fast-path now keeps Running, so wake inside
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
        yield_now();
    }
    exit_current(0)
}

/// Explicit ABI-stable entry point for the second smoke task.
#[cfg(feature = "selftest")]
extern "C" fn smoke_task_b() -> ! {
    for _ in 0..SMOKE_ITERS {
        SerialPort::puts("[task] B\n");
        yield_now();
    }
    exit_current(1)
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
