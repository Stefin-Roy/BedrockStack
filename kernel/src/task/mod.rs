//! Cooperative task scheduler.
//!
//! A single global FIFO run queue of kernel tasks is switched by `switch_to`.
//! The scheduler runs on the BSP only (APs idle in `ap_entry64`), so no
//! per-CPU queues or cross-CPU locking are needed yet.  Tasks are
//! `Box::leak`ed to `&'static mut`; exiting tasks are parked into `DEAD_TASKS`
//! and reclaimed from the idle loop by `reap_dead` (root page tables,
//! kernel stacks, and the task boxes).  The idle anchor is the run()/idle
//! context, captured on the first switch away and restored when the queue
//! empties.
//!
//! User-mode entry (`enter_userspace`) builds an iretq frame and an initial
//! context pointing at `user_iret`; it is not exercised until a user program
//! loader exists (Phase 6).

mod switch;
pub mod load;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::mm::layout::{KSTACK_SIZE, KSTACK_VADDR_BASE, MAX_KSTACKS};
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};
use switch::{switch_to, user_iret_addr};

pub use switch::TaskContext;

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
    /// Page-table root (CR3) this task runs in.
    pub root: u64,
    pub user_gs: u64,
    pub ctx: TaskContext,
}

impl Task {
    pub const fn new(kernel_stack_top: u64, root: u64, user_gs: u64, ctx: TaskContext) -> Self {
        Task {
            id: 0,
            state: TaskState::Ready,
            kernel_stack_top,
            kstack_slot: usize::MAX,
            root,
            user_gs,
            ctx,
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
    for i in 0..KSTACK_PAGES {
        let pa = alloc.alloc()?;
        unsafe {
            core::ptr::write_bytes(crate::mm::layout::to_physmap(pa) as *mut u8, 0, 4096);
        }
        let va = base + (i as u64) * 4096;
        vmm.map_4k(alloc, va, pa, PageFlags::READ | PageFlags::WRITE);
    }
    in_use[slot] = true;
    Some((base + KSTACK_SIZE, slot))
}

/// Unmap and free the four frames backed by window `slot`, releasing the slot.
///
/// Runs on the idle stack under the kernel root (from `reap_dead`), so the
/// stack is long dead and no CPU parks on it.  `Vmm::unmap_4k` flushes and
/// broadcasts a TLB shootdown before any frame is freed.
fn free_kernel_stack(slot: usize, alloc: &mut BitmapAllocator) {
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
    for i in 0..KSTACK_PAGES {
        let va = base + (i as u64) * 4096;
        if let Some(phys) = vmm.translate(va) {
            vmm.unmap_4k(alloc, va);
            unsafe { alloc.free(phys); }
        }
    }
}

/// Global FIFO of runnable tasks.
static QUEUE: Mutex<VecDeque<&'static mut Task>> = Mutex::new(VecDeque::new());

/// The task currently running on this CPU, or `None` when idle.
static CURRENT: Mutex<Option<&'static mut Task>> = Mutex::new(None);

/// Parked `Dead` tasks awaiting reclamation.
///
/// Pushed at every switch-away site for a Dead task, then drained by
/// `reap_dead` from the idle loop.  A dead task cannot free its own stack
/// (its CTX still points into it and the CPU parked on it), so teardown is
/// deferred until the scheduler is back on the idle stack.  `reap_dead` only
/// takes this lock (never `QUEUE`), and the idle loop never runs `schedule`,
/// so there is no lock ordering against `QUEUE`.
static DEAD_TASKS: Mutex<Vec<&'static mut Task>> = Mutex::new(Vec::new());

/// Anchor context: the idle (run()/scheduler) register state. Captured by the
/// first `switch_to` away from it; restored when no ready task remains.
static mut IDLE: Task = Task::new(0, 0, 0, TaskContext::zeroed());

/// Kernel page-table root. Kernel threads share it; it is also the root to
/// restore when parking an exiting task into idle.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1
}

fn idle_ctx() -> *mut TaskContext {
    // The idle anchor task lives in a static; writing its context is inherent
    // to capturing/restoring the scheduler register state.
    unsafe { core::ptr::addr_of_mut!(IDLE.ctx) }
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

/// Enqueue a task for the scheduler. Returns its id.
pub fn spawn(mut task: Task) -> u64 {
    task.id = next_id();
    task.state = TaskState::Ready;
    let leaked: &'static mut Task = Box::leak(Box::new(task));
    let id = leaked.id;
    QUEUE.lock().push_back(leaked);
    id
}

/// Cooperative yield: move the current task to the tail of the run queue and
/// run the next ready task, if any.
pub fn yield_now() {
    schedule();
}

/// Mark the current task Dead and switch to the next ready task. A Dead task
/// is never requeued, so this never returns.
pub fn exit_current(code: u64) -> ! {
    log::info!("[sched] task exit({})", code);
    SerialPort::puts("[sched] task exit(code=");
    SerialPort::put_u64(code);
    SerialPort::puts(")\n");
    kill_current()
}

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
    SerialPort::puts("[sched] user fault — killing current task\n");
    kill_current()
}

/// Point the current CPU's TSS.rsp0 and PerCpu.syscall_rsp0 at `top`, so
/// interrupts and syscalls land on the running task's kernel stack.
pub fn set_kernel_stack_meta(top: u64) {
    crate::arch::x86_64::gdt::set_kernel_stack(top);
    crate::smp::current_per_cpu().syscall_rsp0 = top;
}

/// Reclaim parked dead tasks: destroy their private page tables, free their
/// kernel stacks, and drop their `Task` boxes.
///
/// Called from the idle loop, which runs on the boot stack under the kernel
/// root — so the task being reaped is never the calling context, and its user
/// root is no longer active on any CPU (the scheduler is BSP-only, and the
/// park switch already reloaded CR3 to the kernel root, flushing the BSP's
/// TLB of the dead root's entries).
///
/// TSS.rsp0 may still point at a freed stack after this; that is safe because
/// rsp0 is only consumed on a ring-3→ring-0 transition, and no user task runs
/// while the BSP is idling.  The next task switch re-pins rsp0.
pub fn reap_dead(alloc: &mut BitmapAllocator) {
    let mut dead = DEAD_TASKS.lock();
    for task in dead.drain(..) {
        let root = task.root;
        if root != 0 && root != kernel_root() {
            crate::mm::vmm::destroy_root(root, alloc);
        }
        if task.kstack_slot != usize::MAX {
            free_kernel_stack(task.kstack_slot, alloc);
        }
        let raw = &mut *task as *mut Task;
        unsafe { drop(Box::from_raw(raw)); }
    }
}

/// Cooperative scheduler: switch to the next ready task.
///
/// The outgoing task is requeued only if it is still `Running` (a voluntary
/// yield). A `Dead` task (from `exit_current`) is parked and the scheduler
/// drops to idle. No spin lock is held across `switch_to`.
pub fn schedule() {
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
            crate::smp::current_per_cpu().current_task = t as *mut Task as *mut core::ffi::c_void;
            *CURRENT.lock() = Some(t);
            (ctx_ptr, root)
        }
        None => {
            *CURRENT.lock() = None;
            crate::smp::current_per_cpu().current_task = core::ptr::null_mut();
            // No ready task.
            match prev {
                Some(p) if p.state == TaskState::Dead => {
                    // Park the exiting task and resume idle.  The task is
                    // pushed into DEAD_TASKS (after `drop(q)`, so DEAD_TASKS
                    // is never acquired while QUEUE is held) for a later idle
                    // loop reap; `pctx` stays valid — the vec owns the task.
                    let pctx = core::ptr::addr_of_mut!(p.ctx);
                    let root = KERNEL_ROOT.load(Ordering::Relaxed);
                    drop(q);
                    DEAD_TASKS.lock().push(p);
                    unsafe { switch_to(pctx, idle_ctx(), root); }
                    return;
                }
                Some(p) => {
                    // Self-yield with an empty queue: requeue and return
                    // without switching.
                    p.state = TaskState::Ready;
                    q.push_back(&mut *p);
                    drop(q);
                    return;
                }
                None => {
                    drop(q);
                    return;
                }
            }
        }
    };

    match prev {
        // A Dead task may also switch straight to the next ready task (queue
        // non-empty) — same deferred-reap push applies, after `drop(q)`.
        Some(p) if p.state == TaskState::Dead => {
            let pctx = core::ptr::addr_of_mut!(p.ctx);
            drop(q);
            DEAD_TASKS.lock().push(p);
            unsafe { switch_to(pctx, next_ptr, next_root); }
        }
        Some(p) => {
            let pctx = core::ptr::addr_of_mut!(p.ctx);
            p.state = TaskState::Ready;
            q.push_back(p);
            drop(q);
            unsafe { switch_to(pctx, next_ptr, next_root); }
        }
        None => {
            drop(q);
            unsafe { switch_to(idle_ctx(), next_ptr, next_root); }
        }
    }
}

/// Launch a task directly into ring 3 (used once a loader has built a user
/// address space). Builds an iretq frame on the task's kernel stack, programs
/// the kernel/user GS pair, then switches away from idle into the new task.
///
/// Returns only when the launched task has exited and been parked back into
/// idle (the resumed caller then owns the idle loop). A live task never
/// returns through this function — it runs until `exit_current` parks it.
pub fn enter_userspace(
    entry: u64,
    user_stack_top: u64,
    root: u64,
    user_gs: u64,
    alloc: &mut BitmapAllocator,
) {
    // This task gets its own slot in the fixed kernel-stack window; the iretq
    // frame lives on top of it.
    let (kernel_stack_top, slot) = alloc_kernel_stack(alloc).expect("enter_userspace: no kernel stack slot");
    // 5-word iretq frame at the top of the kernel stack (RIP, CS, RFLAGS,
    // RSP, SS) — `user_iret` pops exactly this.
    let frame_base = kernel_stack_top - 40;
    unsafe {
        *(frame_base as *mut u64) = entry;          // RIP
        *(frame_base as *mut u64).add(1) = 0x2B;    // user CS
        *(frame_base as *mut u64).add(2) = 0x202;   // RFLAGS: IF set
        *(frame_base as *mut u64).add(3) = user_stack_top;
        *(frame_base as *mut u64).add(4) = 0x23;    // user SS
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
    task.id = next_id();
    task.state = TaskState::Running;
    let t: &'static mut Task = Box::leak(Box::new(task));
    let ctx_ptr = core::ptr::addr_of_mut!(t.ctx);
    set_kernel_stack_meta(kernel_stack_top);
    crate::smp::current_per_cpu().current_task = t as *mut Task as *mut core::ffi::c_void;
    *CURRENT.lock() = Some(t);

    unsafe {
        switch_to(idle_ctx(), ctx_ptr, root);
    }
}

// ── Boot smoke test ────────────────────────────────────────────────
//
// Two kernel-only tasks alternate on serial, proving the context switch works
// before any user mode exists. Runs once at boot and exits into idle (and
// gets reaped from the idle loop).

const SMOKE_ITERS: u32 = 5;

/// Explicit ABI-stable entry point for the first smoke task.  These must not
/// be local closures: a closure coerced to `fn()` enters through a compiler
/// generated `FnOnce` shim, but `switch_to` starts execution from a fabricated
/// context rather than from a normal call frame.
extern "C" fn smoke_task_a() -> ! {
    for _ in 0..SMOKE_ITERS {
        SerialPort::puts("[task] A\n");
        yield_now();
    }
    exit_current(0)
}

/// Explicit ABI-stable entry point for the second smoke task.
extern "C" fn smoke_task_b() -> ! {
    for _ in 0..SMOKE_ITERS {
        SerialPort::puts("[task] B\n");
        yield_now();
    }
    exit_current(1)
}

/// Spawn two kernel-only tasks that alternate on serial, then run the
/// scheduler. Returns to the caller (idle) once both tasks have exited.
pub fn smoke_test(alloc: &mut BitmapAllocator) {
    let root = KERNEL_ROOT.load(Ordering::Relaxed);

    // Each smoke task runs on its own slot in the fixed kernel-stack window
    // (uniform with every other task, and reaped with it once parked).
    let (top_a, slot_a) = alloc_kernel_stack(alloc).expect("smoke: kernel stack slots exhausted");
    let (top_b, slot_b) = alloc_kernel_stack(alloc).expect("smoke: kernel stack slots exhausted");
    // Entry RSP must be 8 mod 16 (SysV callee entry) — top minus 8.
    let mut ta = Task::new(
        top_a, root, 0,
        TaskContext::new(top_a - 8, smoke_task_a as *const () as usize as u64),
    );
    ta.kstack_slot = slot_a;
    let mut tb = Task::new(
        top_b, root, 0,
        TaskContext::new(top_b - 8, smoke_task_b as *const () as usize as u64),
    );
    tb.kstack_slot = slot_b;
    spawn(ta);
    spawn(tb);

    SerialPort::puts("[task] smoke test starting\n");
    schedule();
    SerialPort::puts("[task] smoke test done\n");
}
