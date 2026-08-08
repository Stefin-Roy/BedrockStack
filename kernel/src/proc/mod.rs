//! Process management and cooperative multitasking (x86_64).
//!
//! Runnable tasks live in a 3-level priority queue (0..=2, higher = more
//! urgent): the scheduler picks the highest non-empty level and round-robins
//! within it, so a priority-2 task always runs ahead of a priority-1/0 one.
//! Each task is a ring-3 program loaded into its own address space (a
//! higher-half clone with an empty low half, §8.14) and endowed with a
//! positional capability table. The scheduler loop (`schedule_cpu`, one per
//! CPU) picks a Runnable task, restores its parked `UserFrame` and sysrets
//! into it; when the task yields, sleeps, joins or exits inside a syscall, it
//! re-parks its frame in its TCB and control returns to the same loop. With
//! the queue empty the loop idles: it polls the xHCI controller for hot-plug
//! and halts with IRQs enabled (a timer wake re-queues a sleeping task).
//!
//! Task teardown reclaims more than the address space: `teardown_task` clears
//! the task's capability table (dropping every delegated node reference) and
//! then `reap_zombies` drops the TCBs of terminated tasks whose last strong
//! ref is the `all_tasks` registry itself. The task's `&'static Domain`
//! allocation is intentionally kept for the kernel lifetime — `TableNode`
//! (obj/table.rs) holds `&'static` back-references into task tables — but its
//! handles and TCB are fully released.
//!
//! Tasks are fully cooperative and single-threaded: no kernel blocking and no
//! preemption, so a task is never resumable in the middle of a syscall. The
//! dedicated per-CPU syscall stack remains the brief ring-0 context for the
//! handler window. Each CPU runs its own cooperative scheduler loop
//! (`schedule_cpu`): it pops the highest-priority Runnable task from its own
//! queue — stealing from other CPUs when empty, and idling (halt) when nothing
//! is left — and resumes it via `sysret`. A task only ever moves between CPUs
//! at a park boundary: `task.cpu` records the CPU that last picked it up, and
//! `push_task` routes re-queues (wake callbacks, yields, spawns) by that
//! affinity, nudging an idle target with the scheduler-wake IPI (vector 53).
//! Kills are deferred: `kill_task` only flags a task that may be running on
//! another CPU, and the target tears itself down at its next park or when a
//! scheduler pops it from a queue.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use spin::Once;

use crate::arch::x86_64::gdt;
use crate::arch::x86_64::syscall::UserFrame;
use crate::drivers::serial::SerialPort;
use crate::mm::elf::{self, ElfError};
use crate::mm::heap;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};
use crate::obj::cap_handle::{CapHandle, CapId, HandleState};
use crate::obj::domain::{self, Domain};
use crate::obj::rights::{CapRights, ContractRights, Rights};
use crate::services::irqsafe::IrqLock;
use crate::services::lockorder;
use crate::services::universal_timer::{
    self, universal_timer_impl, UniversalTimer,
};
use crate::smp::{CpuState, MAX_CPUS, cpu_count, current_cpu_id, per_cpu_by_id};

pub(crate) mod contracts;
mod task;

pub use task::{Continuation, Task, TASK_RUNNABLE, TASK_SLEEPING, TASK_ZOMBIE};

/// The per-task async block-I/O state machine. Set by the block layer's
/// syscall path; consumed by the same path on re-entry; flipped to `Done`
/// by the device ISR completion callback.
pub enum IoState {
    Idle,
    InFlight,
    Done(IoOutcome),
}

/// The recorded outcome of an async block-I/O request.
#[derive(Clone, Copy)]
pub struct IoOutcome {
    pub completed: u32,
    pub errors: u32,
}

/// User stack: 64 KB, located near the top of the low canonical half. The init
/// demo holds a 4 KB invoke-reply buffer plus several descriptor arrays in its
/// `_start` frame, so the 8 KB budget was overflowed; 64 KB leaves ample room
/// for nested helper frames.
const USER_STACK_SIZE: usize = 64 * 1024;
const USER_STACK_TOP: u64 = 0x7FFF_FFFF_F000;

/// Domain ids of the two boot tasks: the demo (init) and the concurrency
/// worker. Ring-3-spawned children get fresh ids from the allocator.
const TASK_INIT_ID: u32 = 100;
const TASK_WORKER_ID: u32 = 101;

/// Next domain id for a ring-3-spawned task.
static NEXT_TASK_ID: AtomicU32 = AtomicU32::new(TASK_WORKER_ID + 1);

fn next_task_id() -> u32 {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}

/// The capability endowment handed to a task at creation.
///
/// The process ABI is positional: the first nine slots of the task domain's
/// table are these capabilities, in insertion order (0=serial, 1=mount,
/// 2=registry, 3=physmem, 4=heap, 5=addrspace, 6=block, 7=table, 8=proc).
///
/// Slot 7 is a `TableNode` over the process's OWN table, endowed with
/// INVOKE|QUERY only (no REVOKE), so `delegate` moves the process's own caps
/// and family cascade-revocation is unreachable from ring 3. Slot 8 is the
/// `proc:task` root node (spawn/yield/kill/join), the Phase A multitask seam.
#[derive(Clone, Copy)]
pub struct ProcessEndowment {
    pub serial: CapId,
    pub mount: CapId,
    pub registry: CapId,
    pub physmem: CapId,
    pub heap: CapId,
    pub addrspace: CapId,
    pub block: CapId,
    pub table: CapId,
    pub proc: CapId,
}

static PROCESS_ENDOWMENT: Once<ProcessEndowment> = Once::new();

/// The init (task 100) process's capability endowment, once booted.
pub fn process_endowment() -> &'static ProcessEndowment {
    PROCESS_ENDOWMENT.get().expect("process endowment not set")
}

// ── Scheduler state ────────────────────────────────────────────────────
//
// All scheduler locks are IrqLocks (spin + local IRQ disable): the wake
// callback runs in the timer ISR and re-queues a sleeping task. Lock order
// (see `lockorder`): the timer tick holds the timer queue while wake runs, so
// RUN_QUEUE sits above TIMER_QUEUE. The scheduler locks are never nested with
// each other (always separate scopes), so the only nested path is
// TIMER_QUEUE → RUN_QUEUE inside the wake callback.

/// Run-queue levels. `Task::priority` is clamped to `N_PRIO - 1` on enqueue.
const N_PRIO: usize = 3;

/// The scheduler's runnable pool: one FIFO queue per priority level plus a
/// mask tracking which levels are non-empty. The scheduler picks the highest
/// non-empty level (`runnable_mask`'s lowest set bit) and round-robins inside
/// it, so a level-2 task always runs ahead of level 1/0.
struct PriorityQueues {
    queues: [VecDeque<Arc<Task>>; N_PRIO],
    /// Bit `p` set iff `queues[p]` is non-empty.
    runnable_mask: u8,
}

impl PriorityQueues {
    fn new() -> Self {
        PriorityQueues {
            queues: core::array::from_fn(|_| VecDeque::new()),
            runnable_mask: 0,
        }
    }

    /// Enqueue at `task.priority`, clamped to the top level.
    fn push(&mut self, task: Arc<Task>) {
        let p = task.priority.min(N_PRIO as u8 - 1) as usize;
        self.queues[p].push_back(task);
        self.runnable_mask |= 1 << p;
    }

    /// Pop the front of the highest non-empty level, if any.
    fn pop_highest(&mut self) -> Option<Arc<Task>> {
        if self.runnable_mask == 0 {
            return None;
        }
        let p = self.runnable_mask.trailing_zeros() as usize;
        let task = self.queues[p].pop_front();
        if self.queues[p].is_empty() {
            self.runnable_mask &= !(1 << p);
        }
        task
    }

    /// True iff no runnable task is queued at any level.
    fn is_empty(&self) -> bool {
        self.runnable_mask == 0
    }
}

struct CpuSched {
    /// Tasks parked as Runnable, per priority level.
    run_queue: IrqLock<PriorityQueues>,
    /// The task currently executing on this CPU (the one the syscall handler
    /// parks).
    current: IrqLock<Option<Arc<Task>>>,
    /// Set while this CPU is parked in `idle_cpu`'s halt. `push_task` reads it
    /// to decide whether to nudge the target with an IPI.
    idle: AtomicBool,
}

struct Scheduler {
    /// One scheduler per CPU; `id` indexes `cpus`.
    cpus: [CpuSched; MAX_CPUS],
    /// Every task ever spawned (appended only): keeps the TCB alive for cap
    /// resolution and forensics, like the boot/driver domain registry.
    all_tasks: IrqLock<Vec<Arc<Task>>>,
}

impl Scheduler {
    fn cpu(&self, id: u32) -> &CpuSched {
        &self.cpus[id as usize]
    }
}

/// Enqueue a runnable task on its affinity CPU's run queue, nudging that CPU's
/// scheduler if it is parked idle. Safe from ISR context (the wake callback
/// uses it); holds the queue lock only for the push and drops it before
/// reading `idle` / sending the IPI.
fn push_task(task: Arc<Task>) {
    let target = task.cpu.load(Ordering::Relaxed) as usize;
    let wake = {
        let mut q = scheduler().cpu(target as u32).run_queue.lock();
        q.push(task);
        scheduler().cpu(target as u32).idle.load(Ordering::Relaxed)
    };
    if wake {
        sched_wake_ipi(target as u32);
    }
}

/// Ask an idle CPU to re-check its run queue (wakes its `halt`).
fn sched_wake_ipi(cpu: u32) {
    let apic_id = per_cpu_by_id(cpu).apic_id;
    crate::platform::x86_64_pc::apic::send_ipi(apic_id, crate::platform::x86_64_pc::apic::IPI_SCHED);
}

static SCHEDULER: Once<&'static Scheduler> = Once::new();

fn scheduler() -> &'static Scheduler {
    *SCHEDULER.get().expect("proc: scheduler not initialized")
}

/// True once `boot_tasks` has spawned the boot tasks and the scheduler is
/// live. Syscall handlers use this to pick the scheduler-aware path.
static SCHEDULER_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn scheduler_active() -> bool {
    SCHEDULER_ACTIVE.load(Ordering::Acquire)
}

/// The active kernel page-table root, needed to clone child address spaces.
static KERNEL_ROOT: Once<u64> = Once::new();

// ── Boot: load INIT twice and hand the BSP to the scheduler ────────────

/// Load the init binary from the ESP, spawn the two boot tasks (init = 100,
/// worker = 101), and register the `proc:task` contract. Does NOT enter user
/// mode; the caller must then call `schedule_cpu(0)`.
pub fn boot_tasks(alloc: &mut BitmapAllocator, kernel_root: u64) -> Result<(), &'static str> {
    SerialPort::puts("[proc] Loading init from ESP...\n");

    let init_data = read_init_from_esp()?;
    SerialPort::puts("[proc] Init binary read, ");
    SerialPort::put_u64(init_data.len() as u64);
    SerialPort::puts(" bytes\n");

    KERNEL_ROOT.call_once(|| kernel_root);

    let sched = alloc::boxed::Box::leak(Box::new(Scheduler {
        cpus: core::array::from_fn(|_| CpuSched {
            run_queue: IrqLock::with_order(PriorityQueues::new(), lockorder::RUN_QUEUE),
            current: IrqLock::with_order(None, lockorder::CURRENT_TASK),
            idle: AtomicBool::new(false),
        }),
        all_tasks: IrqLock::with_order(Vec::new(), lockorder::ALL_TASKS),
    }));
    SCHEDULER.call_once(|| sched);

    contracts::register_proc_contract();

    // Affinity: init is affine to CPU 0 (it runs there after boot), the worker
    // to CPU 1 — CPU 1's idle scheduler steals it from the BSP's queue.
    let init_task = spawn_task(TASK_INIT_ID, &init_data, alloc, kernel_root)?;
    init_task.cpu.store(0, Ordering::Relaxed);
    let worker_task = spawn_task(TASK_WORKER_ID, &init_data, alloc, kernel_root)?;
    worker_task.cpu.store(1, Ordering::Relaxed);

    SCHEDULER_ACTIVE.store(true, Ordering::Release);

    // Wake the APs out of their pre-scheduler `ap_main` wait so they can enter
    // their own scheduler loops and steal the boot tasks. Without this nudge
    // they would stay halted (only interrupts exit `halt`) and the system
    // would silently degrade to BSP-only execution.
    crate::platform::x86_64_pc::apic::send_ipi_all_except_self(
        crate::platform::x86_64_pc::apic::IPI_SCHED,
    );

    Ok(())
}

/// Boot entry: load both boot tasks, then hand the BSP to the scheduler.
/// Never returns — with every queue empty the BSP idles (xHCI hot-plug poll +
/// halt), so a failed init still leaves a live kernel.
pub fn run(alloc: &mut BitmapAllocator, kernel_root: u64) -> ! {
    if let Err(e) = boot_tasks(alloc, kernel_root) {
        log::warn!("boot_tasks failed: {}", e);
        SerialPort::puts("[kernel] boot_tasks failed: ");
        SerialPort::puts(e);
        SerialPort::puts("\n");
    }
    SerialPort::puts("[proc] scheduler: entering schedule_cpu(0)\n");
    schedule_cpu(0);
}

/// AP scheduler entry: wait for the scheduler to come up (boot_tasks on the
/// BSP), then run this CPU's scheduler loop forever. Called from `ap_entry64`.
pub fn ap_main(cpu: u32) -> ! {
    loop {
        // Drain any boot-time work posted before the scheduler comes up (the
        // BSP's parallel device sweep), then wait for the scheduler.
        crate::smp::work::drain(cpu);
        if SCHEDULER_ACTIVE.load(Ordering::Acquire) {
            break;
        }
        crate::arch::CurrentArch::halt();
    }
    schedule_cpu(cpu)
}

/// Endow a fresh task domain with the nine positional process capabilities.
fn endow_task(domain: &'static Domain) -> Result<ProcessEndowment, ()> {
    let boot = crate::obj::bootstrap::boot_domain();
    let boot_end = crate::obj::bootstrap::boot_endowment();
    let serial = boot.table.delegate(&domain.table, boot_end.serial).map_err(|_| ())?;
    let mount = boot.table.delegate(&domain.table, boot_end.mount).map_err(|_| ())?;
    let registry = boot.table.delegate(&domain.table, boot_end.registry).map_err(|_| ())?;
    let physmem = boot.table.delegate(&domain.table, boot_end.physmem).map_err(|_| ())?;
    let heap_cap = boot.table.delegate(&domain.table, boot_end.heap).map_err(|_| ())?;
    let addrspace = boot.table.delegate(&domain.table, boot_end.addrspace).map_err(|_| ())?;
    let block = boot.table.delegate(&domain.table, boot_end.block).map_err(|_| ())?;
    // The process's own table node (not boot's): `delegate` operates on the
    // process's own caps, and no REVOKE is granted, so a ring-3 process can
    // never cascade-sever a family or re-delegate boot's capabilities.
    let table = domain.table.insert(CapHandle {
        id: CapId(0),
        node: crate::obj::table::table_node(&domain.table),
        rights: CapRights::new(
            Rights::INVOKE.or(Rights::QUERY),
            ContractRights::READ.or(ContractRights::WRITE).or(ContractRights::CALL),
        ),
        state: HandleState::Live,
    });
    // The proc:task root node: spawn/yield/kill/join on the caller task.
    let proc = domain.table.insert(CapHandle {
        id: CapId(0),
        node: contracts::proc_root_node(),
        rights: CapRights::new(
            Rights::INVOKE.or(Rights::QUERY),
            ContractRights::READ.or(ContractRights::WRITE).or(ContractRights::CALL),
        ),
        state: HandleState::Live,
    });
    Ok(ProcessEndowment {
        serial,
        mount,
        registry,
        physmem,
        heap: heap_cap,
        addrspace,
        block,
        table,
        proc,
    })
}

/// Spawn a boot task (init or worker) from the same init binary.
fn spawn_task(
    id: u32,
    data: &[u8],
    alloc: &mut BitmapAllocator,
    kernel_root: u64,
) -> Result<Arc<Task>, &'static str> {
    let domain = Domain::with_addrspace(id, kernel_root);
    domain::register_domain(domain);

    let endowment = endow_task(domain).map_err(|_| "endowment failed")?;

    // Boot priorities differ so the level-2 and level-1 paths are both
    // exercised: init (the demo) runs at 2, the worker at 1.
    let priority = if id == TASK_INIT_ID { 2 } else { 1 };
    let task = build_task(id, priority, domain, data, alloc)?;

    if id == TASK_INIT_ID {
        PROCESS_ENDOWMENT.call_once(|| endowment);
    }
    Ok(task)
}

/// Load a task's ELF + user stack into `domain` and enqueue it.
fn build_task(
    id: u32,
    priority: u8,
    domain: &'static Domain,
    data: &[u8],
    alloc: &mut BitmapAllocator,
) -> Result<Arc<Task>, &'static str> {
    let root = domain.page_root().ok_or("no addrspace")?;
    let mut vmm = Vmm::from_root(root);

    let entry = match elf::load_elf(data, &mut vmm, alloc) {
        Ok(e) => e,
        Err(ElfError::NotElf) => return Err("not an ELF"),
        Err(ElfError::Not64Bit) => return Err("not 64-bit"),
        Err(ElfError::NotLittleEndian) => return Err("not little-endian"),
        Err(ElfError::NotExecutable) => return Err("not executable"),
        Err(ElfError::WrongMachine) => return Err("wrong machine type"),
        Err(ElfError::InvalidPhdr) => return Err("invalid program header"),
        Err(ElfError::OutOfMemory) => return Err("out of memory"),
        Err(ElfError::SegmentTooLarge) => return Err("segment too large"),
    };

    // Allocate the user stack (8 KB RW+USER near the top of the low half).
    let stack_bottom = USER_STACK_TOP - USER_STACK_SIZE as u64;
    for page in (stack_bottom..USER_STACK_TOP).step_by(4096) {
        let frame = alloc.alloc().ok_or("OOM for user stack")?;
        vmm.map_4k(alloc, page, frame, PageFlags::READ | PageFlags::WRITE | PageFlags::USER);
    }
    let user_rsp = USER_STACK_TOP - 8;

    let task = Arc::new(Task::new(id, priority, domain, entry, user_rsp));

    push_task(Arc::clone(&task));
    {
        scheduler().all_tasks.lock().push(Arc::clone(&task));
    }

    SerialPort::puts("[proc] task ");
    SerialPort::put_u64(id as u64);
    SerialPort::puts(" loaded, entry at 0x");
    SerialPort::put_hex(entry);
    SerialPort::puts("\n");
    Ok(task)
}

// ── Parking and resumption ─────────────────────────────────────────────

/// Copy the live user frame at the top of the syscall stack into the current
/// task's TCB. Called at the top of every syscall handler: blocking syscalls
/// (yield/sleep/join/exit) resume from this copy; non-blocking syscalls return
/// through the normal asm epilogue and ignore it.
pub fn park_current_frame(frame: *const UserFrame) {
    if !scheduler_active() {
        return;
    }
    let task = current_task_option();
    if let Some(task) = task {
        if task.kill_requested.load(Ordering::Relaxed) {
            // Killed while running on another CPU: die at the next park (this
            // one). Never resumes; the scheduler teardown path takes over.
            teardown_task(&task);
            *scheduler().cpu(current_cpu_id()).current.lock() = None;
            schedule_cpu(current_cpu_id()); // never returns
        }
        // SAFETY: `frame` points at the live UserFrame on the per-CPU syscall
        // stack; the task is the one currently executing on this CPU.
        let mut parked = task.parked.lock();
        parked.frame = unsafe { *frame };
        parked.user_rsp = crate::smp::current_per_cpu().syscall_user_rsp;
    }
}

/// The task currently executing on this CPU, if any.
fn current_task_option() -> Option<Arc<Task>> {
    scheduler().cpu(current_cpu_id()).current.lock().as_ref().cloned()
}

/// The currently executing task, if the scheduler is live (used by the
/// `proc:task` node's surface reads).
pub(crate) fn current_task() -> Option<Arc<Task>> {
    if !scheduler_active() {
        return None;
    }
    current_task_option()
}

/// The currently executing task (kernel-bug panic if none).
fn current_task_arc(who: &str) -> Arc<Task> {
    scheduler().cpu(current_cpu_id()).current.lock().as_ref().cloned().unwrap_or_else(|| {
        panic!("proc: {who} called with no current task")
    })
}

/// Yield the CPU: park the caller as Runnable at the back of the queue.
/// `schedule_cpu` resumes it (possibly immediately if it is the only task).
pub fn yield_current() -> ! {
    let task = current_task_arc("yield");
    task.parked.lock().frame.rax = 0; // the invoking syscall "returns 0"
    task.set_state(TASK_RUNNABLE);
    push_task(Arc::clone(&task));
    *scheduler().cpu(current_cpu_id()).current.lock() = None;
    schedule_cpu(current_cpu_id());
}

/// Park the caller on the universal timer for `ms` ms; the timer ISR wakes it
/// and re-queues it. Always safe, even as the last live task.
pub fn sleep_current(ms: u64) -> ! {
    let task = current_task_arc("sleep");
    task.parked.lock().frame.rax = 0; // the invoking syscall "returns 0"
    let now = universal_timer::now_ns();
    let deadline = now.saturating_add(ms.saturating_mul(1_000_000));
    let entry = Box::into_raw(Box::new(SleepEntry { task: Arc::clone(&task) }));
    let id = universal_timer_impl().set(deadline, wake_callback, entry as *mut u8);
    *task.sleep_timer.lock() = Some(id);
    task.set_state(TASK_SLEEPING);
    *scheduler().cpu(current_cpu_id()).current.lock() = None;
    schedule_cpu(current_cpu_id());
}

/// Park the caller until `child` dies. The child's teardown wakes us (a join
/// wait-list on the child), so this is a real wait, not a poll.
pub fn join_park(child: Arc<Task>) -> ! {
    let task = current_task_arc("join");
    task.parked.lock().frame.rax = 0; // the invoking syscall "returns 0"
    {
        child.joiners.lock().push(Arc::clone(&task));
    }
    task.set_state(TASK_SLEEPING);
    *scheduler().cpu(current_cpu_id()).current.lock() = None;
    schedule_cpu(current_cpu_id());
}

/// Park the current task off the run queue until its async I/O completes.
/// The completion callback (`wake_io_complete`) re-queues it; the scheduler
/// then runs `cont` against the parked frame before resuming user mode.
///
/// The task MUST park as `TASK_SLEEPING`, never Runnable: a Runnable park
/// with an otherwise-empty queue would make the scheduler re-resume it
/// immediately, re-running the continuation before the device ISR fires —
/// with IRQs disabled in the syscall path, that deadlocks. Sleeping +
/// ISR-wake guarantees the scheduler reaches its idle `halt` (interrupts
/// enabled) so the completion IRQ actually fires.
pub fn park_async_retry(cont: Continuation) -> ! {
    let task = current_task_arc("async");
    task.parked.lock().continuation = Some(cont);
    task.set_state(TASK_SLEEPING);
    *scheduler().cpu(current_cpu_id()).current.lock() = None;
    schedule_cpu(current_cpu_id());
}

/// The exit syscall: record the code, tear the current task down, and hand the
/// CPU back to the scheduler (which idles when the queue is empty).
pub fn exit_process(code: i64) -> ! {
    SerialPort::puts("[proc] task exiting with code ");
    SerialPort::put_u64(code as u64);
    SerialPort::puts("\n");

    let task = current_task_arc("exit");
    teardown_task(&task);
    *scheduler().cpu(current_cpu_id()).current.lock() = None;
    schedule_cpu(current_cpu_id());
}

/// Kill a task by its TCB (via a `proc:task` child cap). SMP-safe: the target
/// may be RUNNING on another CPU, so teardown must NOT happen while it
/// executes. A self-kill falls through the exit path; a foreign kill is
/// deferred — the target tears itself down at its next park, or when an
/// idle/stealing CPU pops it from a queue.
pub fn kill_task(target: &Arc<Task>) {
    if target.is_zombie() || target.kill_requested.load(Ordering::Relaxed) {
        return;
    }
    target.kill_requested.store(true, Ordering::Relaxed);
    let is_self = {
        let cur = scheduler().cpu(current_cpu_id()).current.lock().as_ref().cloned();
        matches!(&cur, Some(c) if Arc::ptr_eq(c, target))
    };
    if is_self {
        teardown_task(target);
        *scheduler().cpu(current_cpu_id()).current.lock() = None;
        schedule_cpu(current_cpu_id());
    }
    // Non-self: deferred. The target tears itself down at its next park
    // (park_current_frame) or when an idle/stealing CPU pops it from a queue
    // (schedule_cpu). Nudge its CPU so an idle scheduler notices promptly.
    else {
        sched_wake_ipi(target.cpu.load(Ordering::Relaxed));
    }
}

/// Mark a task Zombie, recover its pending sleep entry, reclaim its user
/// address space, release the domain's capability handles, and wake any
/// joiners parked on it.
fn teardown_task(task: &Arc<Task>) {
    task.set_state(TASK_ZOMBIE);

    // Recover a pending sleep entry (if any) so its strong ref is dropped and
    // the one-shot timer no longer references it.
    if let Some(id) = task.sleep_timer.lock().take() {
        if let Some(ctx) = universal_timer_impl().remove_context(id) {
            // SAFETY: the context is the `SleepEntry` box we armed in
            // `sleep_current`, and the timer no longer holds it.
            unsafe { drop(Box::from_raw(ctx as *mut SleepEntry)) };
        }
    }

    // Wake joiners parked on this task's death (they are not zombies; a dead
    // joiner's Arc is simply released).
    let joiners: Vec<Arc<Task>> = { task.joiners.lock().drain(..).collect() };
    for j in &joiners {
        if j.state() != TASK_ZOMBIE {
            j.set_state(TASK_RUNNABLE);
            push_task(Arc::clone(j));
        }
    }

    // Reclaim the task's user address space: free every low-half frame (ELF
    // segments + stack) back to the physical allocator. The high half is
    // shared with the kernel and is never touched.
    if let Some(root) = task.domain.page_root() {
        crate::mm::vmm::teardown_low_half(root, heap::get_phys_allocator_mut());
    }

    // Release every capability handle in the task's table: serial/physmem/
    // block/mount/table/child-TaskNode references all drop here, including the
    // task's self-table-cap and its `proc:task` root cap. Dropping a child
    // `TaskNode` cap decrements the child's `Arc` strong count, which is what
    // lets later `reap_zombies` sweeps reclaim exited children whose parent
    // died. The `&'static Domain` allocation survives (see task.rs), but it
    // no longer references anything.
    task.domain.table.clear();

    // A cascade where a reaped parent drops child caps can free the children
    // only on a later sweep, so reap immediately after every teardown.
    reap_zombies();
}

/// Drop terminated tasks whose last strong reference is the registry itself:
/// a zombie with no waiting joiners and no cap that still references it. The
/// dropped `Arc` frees the TCB (parked frame, sleep timer, joiners); the
/// task's `&'static Domain` allocation survives (see task.rs docs) but its
/// capability table has already been cleared at teardown.
fn reap_zombies() {
    let mut reaped_ids: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    {
        let mut all = scheduler().all_tasks.lock();
        all.retain(|t| {
            if t.is_zombie() && t.joiners.lock().is_empty() && Arc::strong_count(t) == 1 {
                reaped_ids.push(t.id);
                false
            } else {
                true
            }
        });
    }
    if !reaped_ids.is_empty() {
        SerialPort::puts("[proc] reaped ");
        SerialPort::put_u64(reaped_ids.len() as u64);
        SerialPort::puts(" zombie task(s)\n");
    }
}

// ── The scheduler loop ─────────────────────────────────────────────────

/// The per-CPU cooperative scheduler loop. Owns `cpu` forever: pops the
/// highest-priority Runnable task from this CPU's queue (stealing from others
/// when empty), resumes it via sysret, and idles (halt) when there is nothing
/// to run. A task only ever moves between CPUs at a park boundary: `task.cpu`
/// is written when it is picked up, and `push_task` routes re-queues by it.
pub fn schedule_cpu(cpu: u32) -> ! {
    loop {
        let next = {
            scheduler().cpu(cpu).run_queue.lock().pop_highest()
        };
        let task = match next {
            Some(t) => t,
            None => match steal_task(cpu) {
                Some(t) => t,
                None => {
                    idle_cpu(cpu);
                    continue;
                }
            },
        };
        if !task.is_runnable() {
            // Torn down while queued (deferred-killed): drop and pick the next.
            continue;
        }
        if task.kill_requested.load(Ordering::Relaxed) {
            teardown_task(&task);
            continue;
        }

        *scheduler().cpu(cpu).current.lock() = Some(Arc::clone(&task));
        *task.sleep_timer.lock() = None;
        domain::set_current_domain(task.domain);
        task.cpu.store(cpu, Ordering::Relaxed);

        // Copy the parked frame out and drop the guard: an IrqGuard held
        // across `resume_user` would leave IRQs disabled in ring 3, starving
        // the timer that wakes sleeping tasks. The local lives on the syscall
        // stack, which `resume_user` abandons (it switches to the user RSP).
        let (frame, user_rsp) = {
            let mut parked = task.parked.lock();
            if let Some(cont) = parked.continuation.take() {
                parked.frame.rax = cont(&mut parked.frame);
            }
            (parked.frame, parked.user_rsp)
        };
        crate::smp::current_per_cpu().syscall_user_rsp = user_rsp;

        // Switch to ring 3 — never returns. The task's next park re-enters
        // this loop via `syscall_entry` → `syscall_handler`.
        unsafe { resume_user(&frame) }
    }
}

/// Idle: advertise idle, re-check the local queue (lost-wakeup guard), then
/// halt with IRQs enabled until an interrupt (timer wake, device IRQ, or a
/// scheduler-wake IPI from `push_task`) makes us re-check.
fn idle_cpu(cpu: u32) {
    scheduler().cpu(cpu).idle.store(true, Ordering::Release);
    // Re-check after advertising idle: a pusher that read `idle == false`
    // just before this store relies on this re-check to see its push.
    if !scheduler().cpu(cpu).run_queue.lock().is_empty() {
        scheduler().cpu(cpu).idle.store(false, Ordering::Release);
        return;
    }
    if cpu == 0 {
        // Reap zombies only on the BSP's idle (a natural backstop that
        // eventually reclaims any exited child whose parent's teardown dropped
        // the last child-caps).
        reap_zombies();
        #[cfg(target_arch = "x86_64")]
        {
            let new_devices = crate::usb::xhci::poll();
            if !new_devices.is_empty() {
                for dev in new_devices {
                    crate::register_block_device(dev);
                }
            }
        }
    }
    crate::arch::CurrentArch::enable_interrupts();
    crate::arch::CurrentArch::halt();
    crate::arch::CurrentArch::disable_interrupts();
    scheduler().cpu(cpu).idle.store(false, Ordering::Release);
}

/// Steal the highest-priority task from another CPU's queue (round-robin
/// victims, starting at `my_cpu + 1`). Locks exactly one victim queue at a
/// time and never nests it with our own queue lock.
fn steal_task(my_cpu: u32) -> Option<Arc<Task>> {
    let n = cpu_count();
    if n <= 1 {
        return None;
    }
    for off in 1..n {
        let victim = (my_cpu + off) % n;
        if victim == my_cpu || crate::smp::cpu_state(victim) == CpuState::Offline {
            continue;
        }
        let mut vq = scheduler().cpu(victim).run_queue.lock();
        if let Some(t) = vq.pop_highest() {
            return Some(t);
        }
    }
    None
}

/// Transition from ring 0 to ring 3 using `sysretq`, restoring the parked
/// `UserFrame` (the first-entry frame for a fresh task).
///
/// # Safety
/// `frame` must be a valid parked frame for a task whose user RSP has been
/// stashed in `gs:syscall_user_rsp`. GS is deliberately NOT reloaded: the
/// kernel keeps its per-CPU data in GS.base in both ring 3 and ring 0.
#[unsafe(naked)]
unsafe extern "C" fn resume_user(_frame: *const UserFrame) -> ! {
    core::arch::naked_asm!(
        "mov ax, {user_ds}",
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        // Restore the frame from the TCB (rdi = &UserFrame), loading rdi last
        // since it is also the base register.
        "mov rcx, [rdi + {rcx_off}]",
        "mov r11, [rdi + {r11_off}]",
        // Mask user RFLAGS before sysret (mirrors the syscall-entry epilogue).
        "and r11, 0x8D5",
        "or r11, 0x202",
        "mov rsp, gs:[{usr}]",
        "mov rax, [rdi + {rax_off}]",
        "mov rdx, [rdi + {rdx_off}]",
        "mov rsi, [rdi + {rsi_off}]",
        "mov r10, [rdi + {r10_off}]",
        "mov rbx, [rdi + {rbx_off}]",
        "mov rbp, [rdi + {rbp_off}]",
        "mov r12, [rdi + {r12_off}]",
        "mov r13, [rdi + {r13_off}]",
        "mov r14, [rdi + {r14_off}]",
        "mov r15, [rdi + {r15_off}]",
        "mov rdi, [rdi + {rdi_off}]",
        "sysretq",
        user_ds = const gdt::USER_DS_SELECTOR,
        usr = const offset_of!(crate::smp::PerCpu, syscall_user_rsp),
        rcx_off = const offset_of!(UserFrame, rcx),
        r11_off = const offset_of!(UserFrame, r11),
        rax_off = const offset_of!(UserFrame, rax),
        rdx_off = const offset_of!(UserFrame, rdx),
        rsi_off = const offset_of!(UserFrame, rsi),
        r10_off = const offset_of!(UserFrame, r10),
        rbx_off = const offset_of!(UserFrame, rbx),
        rbp_off = const offset_of!(UserFrame, rbp),
        r12_off = const offset_of!(UserFrame, r12),
        r13_off = const offset_of!(UserFrame, r13),
        r14_off = const offset_of!(UserFrame, r14),
        r15_off = const offset_of!(UserFrame, r15),
        rdi_off = const offset_of!(UserFrame, rdi),
    );
}

// ── Sleep wake callback (ISR context) ─────────────────────────────────

/// A sleeping task's timer context: the strong ref that keeps the task alive
/// while it is parked off the run queue.
struct SleepEntry {
    task: Arc<Task>,
}

/// Runs in the timer ISR: recover the entry, mark the task Runnable, and
/// re-queue it. Touches nothing but the run-queue IrqLock (plus the task's
/// atomic state), so it is isr_safe by construction.
fn wake_callback(context: *mut u8) {
    // SAFETY: the context is a live `SleepEntry` box; the one-shot timer
    // consumed it (it is no longer in the queue).
    let entry = unsafe { Box::from_raw(context as *mut SleepEntry) };
    entry.task.set_state(TASK_RUNNABLE);
    push_task(Arc::clone(&entry.task));
    drop(entry);
}

/// ISR-safe completion wake: record the outcome, mark the task runnable and
/// re-queue it. Runs from the AHCI completion callback (device ISR context);
/// touches only the task's IrqLock'd `io_state` + atomic state + the run-queue
/// IrqLock (same isr-safety pattern as `wake_callback`).
pub fn wake_io_complete(task: &Arc<Task>, outcome: IoOutcome) {
    *task.io_state.lock() = IoState::Done(outcome);
    if task.is_zombie() {
        return;
    }
    task.set_state(TASK_RUNNABLE);
    push_task(Arc::clone(task));
}

// ── Ring-3 spawn (proc:task `spawn` hook) ──────────────────────────────

/// Spawn a child task from an ELF image. The child inherits exactly what the
/// parent held: every live cap handle is delegated into the child's table, and
/// the child gets its own address space (fresh higher-half clone) and stack.
pub(crate) fn spawn_child(elf_data: &[u8]) -> Result<Arc<Task>, crate::obj::ObjError> {
    let kernel_root = *KERNEL_ROOT.get().ok_or(crate::obj::ObjError::NotSupported)?;
    let id = next_task_id();
    let parent = current_task_arc("spawn");

    let domain = Domain::with_addrspace(id, kernel_root);
    domain::register_domain(domain);

    // Inherit the parent's capability table: clone every live handle, in slot
    // order (the child's slots 0..N mirror the parent's). No amplification —
    // delegation copies the held rights and state.
    for (cap_id, _, _, _) in parent.domain.table.snapshot() {
        parent
            .domain
            .table
            .delegate(&domain.table, cap_id)
            .map_err(|_| crate::obj::ObjError::OutOfMemory)?;
    }

    let task = build_task(id, parent.priority, domain, elf_data, heap::get_phys_allocator_mut())
        .map_err(|_| crate::obj::ObjError::OutOfMemory)?;
    task.cpu.store(current_cpu_id(), Ordering::Relaxed);
    Ok(task)
}

/// Read the init binary from the mounted ESP (B:\EFI\BEDROCK\INIT).
fn read_init_from_esp() -> Result<Vec<u8>, &'static str> {
    use crate::filesystems::vfs::inode::InodeOps;

    // Get the mounted B: drive (ESP).
    let mount = crate::filesystems::vfs::get_mount('B').ok_or("B: not mounted")?;

    // Get root inode ops from the mount's root dentry.
    let root_inode = mount.root.inode.lock();
    let root_inode_arc = root_inode.as_ref().ok_or("no root inode")?;
    let root_ops: &dyn InodeOps = &*root_inode_arc.ops;

    // Walk B:\EFI\BEDROCK\INIT.
    let efi = root_ops.lookup("EFI").map_err(|_| "EFI not found")?;
    let bedrock = efi.lookup("BEDROCK").map_err(|_| "BEDROCK not found")?;
    let init = bedrock.lookup("INIT").map_err(|_| "INIT not found")?;

    let size = init.size() as usize;
    if size == 0 || size > 16 * 1024 * 1024 {
        return Err("init file invalid size");
    }

    let mut data = alloc::vec![0u8; size];
    init.read_at(0, &mut data).map_err(|_| "read failed")?;

    Ok(data)
}
