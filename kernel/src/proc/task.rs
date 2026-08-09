//! Task control blocks for the cooperative multitask scheduler (x86_64).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use crate::arch::x86_64::syscall::UserFrame;
use crate::obj::domain::Domain;
use crate::proc::stream::StreamNode;
use crate::services::irqsafe::IrqLock;
use crate::services::lockorder;
use crate::services::universal_timer::TimerId;

/// Parked, in the run queue, waiting for the scheduler.
pub const TASK_RUNNABLE: u8 = 0;
/// Parked, awaiting a timer wake (sleep) or a join-target teardown.
pub const TASK_SLEEPING: u8 = 1;
/// Torn down (exited or killed); never scheduled again.
pub const TASK_ZOMBIE: u8 = 2;

/// A syscall continuation: re-runs the parked syscall against the task's
/// saved `UserFrame` (reads the original args out of the frame) and returns
/// the syscall's RAX value.
pub type Continuation = fn(&mut UserFrame) -> u64;

/// A schedulable user task.
///
/// The scheduler runs tasks cooperatively on the BSP: a task is either parked
/// (its `UserFrame` copy lives in the TCB) or executing (the live frame sits
/// on the per-CPU syscall stack and is re-parked at every syscall entry). The
/// `state` is an atomic so the universal-timer wake callback — which runs in
/// the timer ISR — can flip `Sleeping -> Runnable` and re-queue without a
/// scheduler lock.
///
/// The domain is deliberately `&'static`: like the boot/driver domains it
/// stays alive for the kernel lifetime, so the domain registry, `current_domain`
/// and the `find_domain` delegation target stay sound. What IS reclaimed at
/// teardown is the domain's *user address space*: every low-half frame (ELF
/// segments + stack) is freed back to the physical allocator. The domain's
/// capability table is also cleared at teardown (releasing all delegated node
/// references), and the TCB itself may be reaped from the scheduler registry
/// once it is a zombie with no joiners and no remaining capability references.
/// The `Domain` allocation itself is intentionally kept for the kernel lifetime
/// because `TableNode` (obj/table.rs) holds `&'static` back-references into
/// task tables.
///
/// The frame/RSP/sleep-timer fields are `IrqLock`-wrapped because the TCB is
/// shared through `Arc` (the scheduler queue, the join wait-lists and the
/// sleep-timer context all hold strong refs) and the syscall path mutates them.
/// They are only ever touched by the syscall/schedule path — never from an ISR —
/// so they use the ordering-exempt `IrqLock::new` (order 0) and are never held
/// across a ring-3 resume: `schedule` copies the parked frame out, drops the
/// guard, then sysrets (a guard held across `resume_user` would leave IRQs
/// disabled in user mode).
pub struct Task {
    pub id: u32,
    /// Run-queue priority, 0..=2 (higher = more urgent). The scheduler always
    /// picks the highest non-empty level; levels below it stay round-robin.
    pub priority: u8,
    state: AtomicU8,
    /// The CPU this task last ran on / is affine to. `push_task` routes
    /// re-queues by it; `schedule_cpu` stores the picking CPU when it resumes.
    pub cpu: AtomicU32,
    /// Deferred-kill flag: set by `kill_task` when the target may be running on
    /// another CPU. The target tears itself down at its next park, or when a
    /// scheduler pops it from a queue.
    pub kill_requested: AtomicBool,
    /// Parked user registers + user RSP. Refreshed at every syscall entry and
    /// used to resume the task after a cooperative yield/sleep/join.
    pub parked: IrqLock<Parked>,
    /// The task's address space + capability table.
    pub domain: &'static Domain,
    /// The task's standard streams (`io:stream` nodes), also endowed at cap
    /// slots 0/1/2. `sys_write` fds 0/1/2 route through them; stdout/stderr
    /// echo to the console. They are fresh per task (never shared with the
    /// parent) and stay alive as long as the task or a stream cap does.
    pub stdin: Arc<StreamNode>,
    pub stdout: Arc<StreamNode>,
    pub stderr: Arc<StreamNode>,
    /// Pending one-shot sleep timer, while the task is `Sleeping`. Cleared on
    /// resume and on teardown (via `remove_context`).
    pub sleep_timer: IrqLock<Option<TimerId>>,
    /// Tasks parked in `join` waiting for this task to die. Woken by teardown.
    pub joiners: IrqLock<alloc::vec::Vec<Arc<Task>>>,
    /// The per-task async block-I/O state machine. Set by the block layer's
    /// syscall path, consumed by the same path on re-entry, and flipped to
    /// `Done` by the device ISR completion callback (`wake_io_complete`).
    pub io_state: IrqLock<super::IoState>,
}

/// A task's parked registers and user RSP, stashed at every syscall entry.
pub struct Parked {
    pub frame: UserFrame,
    pub user_rsp: u64,
    /// A kernel-side syscall continuation run at resume: re-dispatches the
    /// syscall the task was parked inside (e.g. to collect an async I/O result
    /// that completed while it slept). Runs in ring 0 before `resume_user`; its
    /// return value becomes the syscall's RAX. `None` for a normal park.
    pub continuation: Option<Continuation>,
}

impl Task {
    /// Build a fresh, never-run task with a synthetic first-entry frame. `stdin`/
    /// `stdout`/`stderr` are the task's standard streams (endowed at cap slots
    /// 0/1/2); the task keeps a strong reference to each.
    pub fn new(
        id: u32,
        priority: u8,
        domain: &'static Domain,
        entry: u64,
        user_rsp: u64,
        stdin: Arc<StreamNode>,
        stdout: Arc<StreamNode>,
        stderr: Arc<StreamNode>,
    ) -> Self {
        Task {
            id,
            priority,
            state: AtomicU8::new(TASK_RUNNABLE),
            cpu: AtomicU32::new(0),
            kill_requested: AtomicBool::new(false),
            // Synthetic frame: RIP = entry, RFLAGS = IF only, table version 1.
            // The remaining user registers start zeroed.
            parked: IrqLock::new(Parked {
                frame: UserFrame {
                    r15: 0,
                    r14: 0,
                    r13: 0,
                    r12: 0,
                    rbx: 0,
                    rbp: 0,
                    rcx: entry,
                    r11: 0x202,
                    rax: 0,
                    rdi: 0,
                    rsi: 0,
                    rdx: 0,
                    r10: 1,
                },
                user_rsp,
                continuation: None,
            }),
            domain,
            stdin,
            stdout,
            stderr,
            sleep_timer: IrqLock::new(None),
            joiners: IrqLock::with_order(alloc::vec::Vec::new(), lockorder::JOINERS),
            io_state: IrqLock::new(super::IoState::Idle),
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::Release);
    }

    pub fn is_runnable(&self) -> bool {
        self.state() == TASK_RUNNABLE
    }

    pub fn is_zombie(&self) -> bool {
        self.state() == TASK_ZOMBIE
    }
}
