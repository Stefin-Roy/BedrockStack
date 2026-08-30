use crate::services::KernelServices;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

/// Firmware-described processor identity used during topology discovery.
/// `hardware_id` is an APIC ID on x86_64 and a hart ID on RISC-V. Logical
/// kernel CPU IDs are assigned by `smp::init` after filtering this list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    pub hardware_id: u32,
    pub enabled: bool,
}

/// SMP initialization guard — prevents double-init which would double-start APs,
/// leak stacks, and corrupt the CPU counter.
static SMP_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Per-CPU online state for future hotplug support.
///
/// 0 = Offline, 1 = Starting, 2 = Online, 3 = Failed.
/// The BSP transitions 0→2 in `early_init_bsp`; APs transition 0→1 in
/// `smp::init` then 1→2 in their respective `ap_entry`, or 1→3 when the
/// bounded startup handshake times out.
static CPU_STATES: [AtomicU8; MAX_CPUS] = [
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpuState {
    Offline,
    Starting,
    Online,
    Failed,
}

impl From<u8> for CpuState {
    fn from(v: u8) -> Self {
        match v {
            1 => CpuState::Starting,
            2 => CpuState::Online,
            3 => CpuState::Failed,
            _ => CpuState::Offline,
        }
    }
}

/// Read the current state of a CPU.
pub fn cpu_state(cpu_id: u32) -> CpuState {
    assert!(
        (cpu_id as usize) < MAX_CPUS,
        "cpu_state: cpu {} out of range",
        cpu_id
    );
    CpuState::from(CPU_STATES[cpu_id as usize].load(Ordering::Acquire))
}

/// Transition a CPU's online state.
pub(crate) fn set_cpu_state(cpu_id: u32, new_state: CpuState) {
    assert!(
        (cpu_id as usize) < MAX_CPUS,
        "set_cpu_state: cpu {} out of range",
        cpu_id
    );
    let new_val = match new_state {
        CpuState::Offline => 0,
        CpuState::Starting => 1,
        CpuState::Online => 2,
        CpuState::Failed => 3,
    };
    CPU_STATES[cpu_id as usize].store(new_val, Ordering::Release);

    let bit = 1u32 << cpu_id;
    if new_state == CpuState::Online {
        ONLINE_MASK.fetch_or(bit, Ordering::AcqRel);
    } else {
        ONLINE_MASK.fetch_and(!bit, Ordering::AcqRel);
    }
    CPU_COUNT.store(ONLINE_MASK.load(Ordering::Acquire).count_ones(), Ordering::Release);
}

/// Cache-line-aligned per-AP ready flag, avoiding false sharing between CPUs.
#[repr(align(64))]
pub struct ApReady {
    pub ready: AtomicBool,
}

pub static AP_READY: [ApReady; MAX_CPUS] = [
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
    ApReady {
        ready: AtomicBool::new(false),
    },
];

/// Per-CPU data structure.
///
/// The first field MUST be `self_ptr` pointing to itself — on x86_64 it is
/// accessed via `gs:[0]` (GS.base points to the PerCpu struct), and on RISC-V
/// via the `tp` register.
#[repr(C)]
pub struct PerCpu {
    pub self_ptr: *const PerCpu,
    pub cpu_id: u32,
    pub apic_id: u32,
    pub is_bsp: bool,
    pub started: AtomicU64,
    pub stack_top: u64,
    pub serial_locked: AtomicU64,
    /// Top of the current task's kernel stack; the TSS.rsp0 written by
    /// `gdt::set_kernel_stack` is mirrored here so the syscall entry asm can
    /// grab it from `gs:[offset]` without reloading the TSS.
    pub syscall_rsp0: u64,
    /// Currently running task (opaque to SMP). `AtomicPtr` instead of raw
    /// `*mut` to avoid data-race UB (S5) — BSP is the only writer today, but
    /// readers (syscall, caps) must not race the `switch_to` store.
    pub current_task: core::sync::atomic::AtomicPtr<core::ffi::c_void>,
    /// Set when a reschedule is needed (tick or wake). Checked on IRQ/syscall return.
    pub need_resched: AtomicBool,
    /// Nesting count for preemptive-critical sections. While >0 tick does not preempt.
    pub preempt_count: AtomicU32,
    /// Per-CPU tick counter for scheduler quantum tracking.
    pub sched_ticks: AtomicU64,
    /// True when this CPU's scheduler has been initialized (after task::init).
    pub sched_active: AtomicBool,
}

/// Scheduler-owned state that is indexed by logical CPU.
///
/// This lives beside, rather than inside, the GS-visible `PerCpu` record so
/// the assembly-facing prefix and the syscall offset remain stable while the
/// scheduler grows. All fields are atomic because remote CPUs will eventually
/// inspect or update parts of this record when waking, balancing, or stopping
/// a CPU.
#[repr(C)]
pub struct SchedulerRuntime {
    /// Context pointer used when a CPU returns to its own idle loop.
    pub idle_context: AtomicPtr<core::ffi::c_void>,
    /// Live stack top used by the CPU's idle/TSS bootstrap path.
    pub idle_stack_top: AtomicU64,
    /// UniversalTimer sequence for this CPU's scheduler slice, if armed.
    pub slice_timer_seq: AtomicU64,
    /// UniversalTimer sequence for this CPU's sleeper wake timer, if armed.
    pub sleep_timer_seq: AtomicU64,
    /// Local weighted-round-robin credit.
    pub wrr_credit: AtomicU32,
    /// Number of completed local context switches.
    pub context_switches: AtomicU64,
    /// Monotonic scheduler handoff generation for diagnostics and ownership
    /// validation. It is not a task or timer sequence number.
    pub handoff_generation: AtomicU64,
}

impl SchedulerRuntime {
    pub const fn new() -> Self {
        Self {
            idle_context: AtomicPtr::new(core::ptr::null_mut()),
            idle_stack_top: AtomicU64::new(0),
            slice_timer_seq: AtomicU64::new(0),
            sleep_timer_seq: AtomicU64::new(0),
            wrr_credit: AtomicU32::new(0),
            context_switches: AtomicU64::new(0),
            handoff_generation: AtomicU64::new(0),
        }
    }
}

static SCHEDULER_RUNTIME: [SchedulerRuntime; MAX_CPUS] =
    [const { SchedulerRuntime::new() }; MAX_CPUS];

pub fn scheduler_runtime(cpu_id: u32) -> &'static SchedulerRuntime {
    assert!(cpu_id < MAX_CPUS as u32, "scheduler_runtime: CPU out of range");
    &SCHEDULER_RUNTIME[cpu_id as usize]
}

pub fn current_scheduler_runtime() -> &'static SchedulerRuntime {
    scheduler_runtime(current_cpu_id())
}

/// Reset scheduler-owned runtime state for a CPU before it is admitted to the
/// scheduler. This is intentionally separate from `PerCpu` initialization so
/// AP startup and future hotplug can reuse the same reset protocol.
pub fn reset_scheduler_runtime(cpu_id: u32, idle_stack_top: u64) {
    let rt = scheduler_runtime(cpu_id);
    rt.idle_context
        .store(core::ptr::null_mut(), Ordering::Release);
    rt.idle_stack_top.store(idle_stack_top, Ordering::Release);
    rt.slice_timer_seq.store(0, Ordering::Release);
    rt.sleep_timer_seq.store(0, Ordering::Release);
    rt.wrr_credit.store(3, Ordering::Release);
    rt.context_switches.store(0, Ordering::Release);
    rt.handoff_generation.store(0, Ordering::Release);
}

/// Byte offset of `PerCpu::syscall_rsp0` within the struct, for the syscall
/// entry asm (`gs:[PERCPU_SYSCALL_RSP0_OFF]`).
pub const PERCPU_SYSCALL_RSP0_OFF: u64 = core::mem::offset_of!(PerCpu, syscall_rsp0) as u64;

/// Max supported CPUs.
pub const MAX_CPUS: usize = 16;

static CPU_COUNT: AtomicU32 = AtomicU32::new(1);
static ONLINE_MASK: AtomicU32 = AtomicU32::new(0);

static mut PER_CPU_SLOTS: [PerCpu; MAX_CPUS] = [
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 0,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 1,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 2,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 3,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 4,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 5,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 6,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 7,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 8,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 9,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 10,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 11,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 12,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 13,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 14,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
    PerCpu {
        self_ptr: core::ptr::null(),
        cpu_id: 15,
        apic_id: 0,
        is_bsp: false,
        started: AtomicU64::new(0),
        stack_top: 0,
        serial_locked: AtomicU64::new(0),
        syscall_rsp0: 0,
        current_task: AtomicPtr::new(core::ptr::null_mut()),
        need_resched: AtomicBool::new(false),
        preempt_count: AtomicU32::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_active: AtomicBool::new(false),
    },
];

#[cfg(target_arch = "x86_64")]
pub fn current_per_cpu() -> &'static mut PerCpu {
    let addr: *mut PerCpu;
    unsafe {
        core::arch::asm!("mov %gs:0, {0}", out(reg) addr, options(att_syntax));
    }
    unsafe { &mut *addr }
}

#[cfg(target_arch = "riscv64")]
pub fn current_per_cpu() -> &'static mut PerCpu {
    let addr: *mut PerCpu;
    unsafe {
        core::arch::asm!("mv {0}, tp", out(reg) addr);
    }
    unsafe { &mut *addr }
}

#[cfg(debug_assertions)]
static SLOT_BUSY: [AtomicU32; MAX_CPUS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Mutable access to one per-CPU slot.
///
/// # Protocol (debug-asserted)
/// - Same-slot re-entrancy is forbidden: `SLOT_BUSY` guards against two live
///   `&mut PerCpu` to the same slot.  Call `slot_release(index)` when done.
fn slot_mut(index: usize) -> &'static mut PerCpu {
    assert!(index < MAX_CPUS, "smp: per-CPU slot {} out of range", index);
    #[cfg(debug_assertions)]
    {
        let prev = SLOT_BUSY[index].fetch_add(1, Ordering::AcqRel);
        assert!(
            prev == 0,
            "smp: slot {} re-entrant mutable access (missing slot_release)",
            index
        );
    }
    unsafe { &mut PER_CPU_SLOTS[index] }
}

/// Read-only access to any per-CPU slot (for cross-CPU observation).
fn slot_read(index: usize) -> &'static PerCpu {
    assert!(index < MAX_CPUS, "smp: per-CPU slot {} out of range", index);
    unsafe { &PER_CPU_SLOTS[index] }
}

#[inline]
fn slot_release(index: usize) {
    #[cfg(debug_assertions)]
    {
        SLOT_BUSY[index].fetch_sub(1, Ordering::Release);
    }
}

/// Returns `Some(PerCpu)` if `early_init_bsp` has been called, else `None`.
pub fn try_current_per_cpu() -> Option<&'static mut PerCpu> {
    let pc = slot_read(0);
    if pc.self_ptr.is_null() {
        None
    } else {
        Some(current_per_cpu())
    }
}

pub fn per_cpu_by_id(cpu_id: u32) -> &'static PerCpu {
    slot_read(cpu_id as usize)
}

pub fn cpu_count() -> u32 {
    CPU_COUNT.load(Ordering::Relaxed)
}

/// Bit mask of CPUs currently in `CpuState::Online`.
pub fn online_mask() -> u32 {
    ONLINE_MASK.load(Ordering::Acquire)
}

/// True if the logical CPU is currently online and may receive IPIs.
pub fn is_cpu_online(cpu_id: u32) -> bool {
    cpu_id < MAX_CPUS as u32 && (online_mask() & (1u32 << cpu_id)) != 0
}

pub fn current_cpu_id() -> u32 {
    current_per_cpu().cpu_id
}

/// Initialize the BSP's per-CPU area (called very early, before heap).
///
/// # Safety
/// Must be called exactly once on the BSP before any SMP operations.
pub unsafe fn early_init_bsp() {
    let pc = slot_mut(0);
    pc.self_ptr = pc as *const PerCpu;
    pc.cpu_id = 0;
    pc.apic_id = 0;
    pc.is_bsp = true;
    pc.started.store(1, Ordering::Relaxed);
    pc.syscall_rsp0 = 0;
    pc.current_task.store(core::ptr::null_mut(), Ordering::Relaxed);
    pc.need_resched.store(false, Ordering::Relaxed);
    pc.preempt_count.store(0, Ordering::Relaxed);
    pc.sched_ticks.store(0, Ordering::Relaxed);
    pc.sched_active.store(false, Ordering::Relaxed);
    reset_scheduler_runtime(0, 0);

    set_cpu_state(0, CpuState::Online);

    #[cfg(target_arch = "x86_64")]
    set_gs_base(pc as *const PerCpu as u64);

    #[cfg(target_arch = "riscv64")]
    set_tp(pc as *const PerCpu);

    // The BSP's slot is fully initialised; release the debug lease so later
    // `slot_mut(0)` callers (e.g. `set_bsp_hardware_id`) don't trip the
    // re-entrancy assert.
    slot_release(0);
}

// ── Preemptive scheduler PerCpu helpers ───────────────────────────────

/// Disable preemption on this CPU (nesting). Must be paired with `preempt_enable`.
#[inline]
pub fn preempt_disable() {
    let pc = current_per_cpu();
    pc.preempt_count.fetch_add(1, Ordering::Relaxed);
    // compiler fence prevents reordering of critical section
    core::sync::atomic::compiler_fence(Ordering::Acquire);
}

/// Enable preemption. If this drops count to zero and need_resched is set, caller should resched.
#[inline]
pub fn preempt_enable() {
    core::sync::atomic::compiler_fence(Ordering::Release);
    let pc = current_per_cpu();
    let prev = pc.preempt_count.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "preempt_enable without disable");
}

/// Enable preemption and if need_resched is pending and we are now
/// preemptible on an active CPU, invoke the scheduler. Returns true if
/// a reschedule was attempted.
///
/// Safe to call before `early_init_bsp` (via `try_current_per_cpu`): if no
/// PerCpu is live it is a no-op and preserves the `prev==0` debug_assert
/// (never fires on `None` — the caller never incremented).
#[inline]
pub fn preempt_enable_and_maybe_resched() -> bool {
    core::sync::atomic::compiler_fence(Ordering::Release);
    let Some(pc) = try_current_per_cpu() else {
        return false;
    };
    let prev = pc.preempt_count.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "preempt_enable without disable");
    #[cfg(target_arch = "x86_64")]
    if prev == 1 && pc.need_resched.load(Ordering::Relaxed) && pc.sched_active.load(Ordering::Acquire) {
        // consume the flag
        if pc.need_resched.swap(false, Ordering::Relaxed) {
            crate::task::maybe_resched_from_preempt();
            return true;
        }
    }
    false
}

/// Returns true when preemption is enabled (count ==0).
#[inline]
pub fn preempt_is_enabled() -> bool {
    let pc = try_current_per_cpu();
    match pc {
        Some(p) => p.preempt_count.load(Ordering::Relaxed) == 0,
        None => true,
    }
}

/// Mark current CPU as needing reschedule.
#[inline]
pub fn set_need_resched() {
    if let Some(pc) = try_current_per_cpu() {
        pc.need_resched.store(true, Ordering::Relaxed);
    }
}

/// Clear need_resched, returns previous value.
#[inline]
pub fn take_need_resched() -> bool {
    if let Some(pc) = try_current_per_cpu() {
        pc.need_resched.swap(false, Ordering::Relaxed)
    } else {
        false
    }
}

/// Test without clearing.
#[inline]
pub fn need_resched() -> bool {
    if let Some(pc) = try_current_per_cpu() {
        pc.need_resched.load(Ordering::Relaxed)
    } else {
        false
    }
}

#[inline]
pub fn inc_sched_ticks() -> u64 {
    let pc = current_per_cpu();
    pc.sched_ticks.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn set_sched_active(cpu: u32, active: bool) {
    let pc = per_cpu_by_id(cpu);
    // SAFETY: per_cpu_by_id returns &'static PerCpu, but sched_active is AtomicBool, no &mut needed
    pc.sched_active.store(active, Ordering::Release);
}

pub fn is_sched_active() -> bool {
    if let Some(pc) = try_current_per_cpu() {
        pc.sched_active.load(Ordering::Acquire)
    } else {
        false
    }
}

#[cfg(target_arch = "x86_64")]
pub fn set_gs_base(addr: u64) {
    use x86_64::registers::model_specific::Msr;
    const IA32_GS_BASE: u32 = 0xC0000101;
    unsafe {
        Msr::new(IA32_GS_BASE).write(addr);
    }
}

#[cfg(target_arch = "riscv64")]
fn set_tp(pc: *const PerCpu) {
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) pc);
    }
}

/// Fill in the hardware ID (APIC ID / hart ID) for the BSP.
pub fn set_bsp_hardware_id(id: u32) {
    let pc = slot_mut(0);
    pc.apic_id = id;
    slot_release(0);
}

// ── lockdep: per-CPU lock-order checking (`lockdep` feature) ──────────
//
// Every `IrqMutex` acquire pushes its order class onto this CPU's stack and
// every release pops it. A class must be strictly greater than everything
// already held on that CPU; a violation panics immediately with the offending
// classes.
//
// Exclusivity argument: `IrqMutex::lock` disables interrupts *before* calling
// [`acquire`] and `IrqGuard::drop` calls [`release`] before re-enabling them,
// so only one context per CPU can touch its stack at any time. No spin lock
// is needed — plain `UnsafeCell` indexed by `current_cpu_id()` suffices, and
// it is safe to use before `early_init_bsp` too (the array is static BSS).
#[cfg(feature = "lockdep")]
pub mod lockdep {
    use super::MAX_CPUS;
    use core::cell::UnsafeCell;

    /// Deepest legal nesting of tracked locks. The scheduler's documented
    /// chains top out at three (KSTACK_IN_USE → CURRENT → QUEUE), but leave
    /// generous headroom for future callers.
    const MAX_HELD: usize = 32;

    struct HeldStack {
        depth: usize,
        classes: [u32; MAX_HELD],
    }

    struct HeldStacks(UnsafeCell<[HeldStack; MAX_CPUS]>);

    unsafe impl Sync for HeldStacks {}

    // Class 0 (LOCKDEP_CLASS_NONE) never reaches here — irq.rs filters it.
    static STACKS: HeldStacks = HeldStacks(UnsafeCell::new(
        [const { HeldStack { depth: 0, classes: [0u32; MAX_HELD] } }; MAX_CPUS],
    ));

    fn cpu_stack() -> &'static mut HeldStack {
        let cpu = super::try_current_per_cpu()
            .map(|pc| pc.cpu_id as usize)
            .unwrap_or(0);
        assert!(cpu < MAX_CPUS, "lockdep: cpu {} out of range", cpu);
        unsafe { &mut (*STACKS.0.get())[cpu] }
    }

    /// Called by `IrqMutex::lock` after IRQs are disabled. Panics on an
    /// ordering violation (acquiring `class` while holding class >= `class`).
    pub fn acquire(class: u32) {
        if class == crate::filesystems::vfs::irq::LOCKDEP_CLASS_NONE {
            return;
        }
        let s = cpu_stack();
        for i in 0..s.depth {
            let held = s.classes[i];
            if held >= class {
                panic!(
                    "lockdep: lock-order violation: acquiring class {} while holding class {} (depth {}, position {})",
                    class, held, s.depth, i
                );
            }
        }
        assert!(
            s.depth < MAX_HELD,
            "lockdep: held-lock stack overflow ({} locks nested)",
            s.depth
        );
        s.classes[s.depth] = class;
        s.depth += 1;
    }

    /// Called by `IrqGuard::drop`. Must mirror the matching `acquire`.
    pub fn release(class: u32) {
        if class == crate::filesystems::vfs::irq::LOCKDEP_CLASS_NONE {
            return;
        }
        let s = cpu_stack();
        assert!(s.depth > 0, "lockdep: release with empty held-lock stack");
        s.depth -= 1;
        assert_eq!(
            s.classes[s.depth], class,
            "lockdep: released class {} but top of stack is class {}",
            class, s.classes[s.depth]
        );
        s.classes[s.depth] = 0;
    }

    /// Debug snapshot: current hold depth on this CPU.
    pub fn depth() -> usize {
        cpu_stack().depth
    }
}

/// Find the PerCpu slot and cpu_id matching a hardware (APIC/hart) ID.
///
/// The returned `&'static mut PerCpu` stays valid (it aliases the static slot
/// table); the debug `SLOT_BUSY` lease is released here because the caller
/// keeps the reference for its own lifetime rather than calling `slot_mut`
/// again.
pub fn find_cpu_by_hardware_id(hw_id: u32) -> Option<(&'static mut PerCpu, u32)> {
    for i in 0..MAX_CPUS {
        let pc = slot_read(i);
        if pc.apic_id == hw_id {
            let slot = slot_mut(i);
            slot_release(i);
            return Some((slot, i as u32));
        }
    }
    None
}

/// Context needed to wake an AP.
pub struct ApContext {
    pub cpu_id: u32,
    pub hardware_id: u32,
    pub stack_top: u64,
}

/// Initialize SMP: discover APs, allocate stacks, start APs.
///
/// Returns the total number of online CPUs.
///
/// # Safety
/// Must be called after heap, page tables, ACPI, and IOAPIC init.
pub unsafe fn init(
    page_table_root: u64,
    acpi: Option<&crate::acpi::AcpiSubsystem>,
    services: &KernelServices,
) -> u32 {
    use crate::drivers::serial::SerialPort;
    assert!(
        !SMP_INITIALIZED.swap(true, Ordering::SeqCst),
        "smp::init() called twice"
    );
    SerialPort::puts("[smp] init\n");

    let discovered = services.cpu.discover_cpus(acpi);
    let bsp_hardware_id = current_per_cpu().apic_id;
    let max_cpus = if crate::bootargs::is_nosmp() {
        1
    } else {
        crate::bootargs::max_cpus()
            .unwrap_or(MAX_CPUS)
            .clamp(1, MAX_CPUS)
    };

    SerialPort::puts("[smp] BSP hardware_id=");
    SerialPort::put_u64(bsp_hardware_id as u64);
    SerialPort::puts(" max_cpus=");
    SerialPort::put_u64(max_cpus as u64);
    SerialPort::puts("\n");

    if crate::bootargs::is_nosmp() {
        SerialPort::puts("[smp] nosmp requested\n");
    }

    let mut ap_list = alloc::vec::Vec::new();
    let mut seen_hardware_ids = alloc::vec::Vec::new();
    seen_hardware_ids.push(bsp_hardware_id);
    for info in discovered.iter() {
        if !info.enabled || info.hardware_id == bsp_hardware_id || ap_list.len() + 1 >= max_cpus {
            continue;
        }
        if seen_hardware_ids.contains(&info.hardware_id) {
            SerialPort::puts("[smp] WARN: duplicate hardware_id=");
            SerialPort::put_u64(info.hardware_id as u64);
            SerialPort::puts(" ignored\n");
            continue;
        }
        seen_hardware_ids.push(info.hardware_id);
        // Logical IDs are assigned densely after filtering disabled entries,
        // the BSP, duplicates, and the max-cpu limit.
        let cpu_id = (ap_list.len() + 1) as u32;
        let hardware_id = info.hardware_id;
        // Skip (don't brick) when no contiguous stack window exists — the
        // machine boots with fewer CPUs rather than hanging forever.
        let stack_top = match allocate_ap_stack(cpu_id) {
            Some(top) => top,
            None => continue,
        };

        set_cpu_state(cpu_id, CpuState::Starting);

        let pc = slot_mut(cpu_id as usize);
        pc.self_ptr = pc as *const PerCpu;
        pc.cpu_id = cpu_id;
        pc.apic_id = hardware_id;
        pc.is_bsp = false;
        pc.started.store(0, Ordering::Relaxed);
        pc.stack_top = stack_top;
        pc.syscall_rsp0 = 0;
        pc.current_task.store(core::ptr::null_mut(), Ordering::Relaxed);
        pc.need_resched.store(false, Ordering::Relaxed);
        pc.preempt_count.store(0, Ordering::Relaxed);
        pc.sched_ticks.store(0, Ordering::Relaxed);
        pc.sched_active.store(false, Ordering::Relaxed);
        slot_release(cpu_id as usize);
        reset_scheduler_runtime(cpu_id, stack_top);
        AP_READY[cpu_id as usize]
            .ready
            .store(false, Ordering::Release);

        ap_list.push(ApContext {
            cpu_id,
            hardware_id,
            stack_top,
        });

        SerialPort::puts("[smp] AP: cpu_id=");
        SerialPort::put_u64(cpu_id as u64);
        SerialPort::puts(" hardware_id=");
        SerialPort::put_u64(hardware_id as u64);
        SerialPort::puts("\n");
    }

    let ap_count = ap_list.len();
    SerialPort::puts("[smp] APs selected: ");
    SerialPort::put_u64(ap_count as u64);
    SerialPort::puts("\n");

    if ap_count == 0 {
        SerialPort::puts("[smp] no APs found, running uniprocessor\n");
        return cpu_count();
    }

    let started = unsafe { services.cpu.wake_aps(page_table_root, &ap_list) };

    SerialPort::puts("[smp] APs started: ");
    SerialPort::put_u64(started as u64);
    SerialPort::puts("\n");

    // AP_READY is only a startup observation. The authoritative count is the
    // online state mask, which prevents timed-out APs from being targeted by
    // TLB/IPI paths or reported as usable CPUs.
    for ap in &ap_list {
        if !AP_READY[ap.cpu_id as usize]
            .ready
            .load(Ordering::Acquire)
            && cpu_state(ap.cpu_id) == CpuState::Starting
        {
            set_cpu_state(ap.cpu_id, CpuState::Failed);
        }
    }
    let online = cpu_count();
    SerialPort::puts("[smp] online CPUs: ");
    SerialPort::put_u64(online as u64);
    SerialPort::puts(" mask=0x");
    SerialPort::put_hex(online_mask() as u64);
    SerialPort::puts("\n");
    online
}

/// Allocate the 17-page contiguous AP stack for one AP.
///
/// Retries a few times (another CPU's `free()` may land between attempts and
/// coalesce a window), then gives up with `None` so `init` can skip that CPU
/// — boot continues uniprocessor-style instead of silently spinning forever.
///
/// Returns the stack **top** physical address (one past the last page).
fn allocate_ap_stack(cpu_id: u32) -> Option<u64> {
    use crate::drivers::serial::SerialPort;
    const AP_STACK_PAGES: usize = 17;
    const AP_STACK_ATTEMPTS: usize = 3;
    let alloc = crate::mm::heap::get_phys_allocator_mut();
    for attempt in 1..=AP_STACK_ATTEMPTS {
        match alloc.try_alloc_contiguous(AP_STACK_PAGES) {
            Ok(base) => return Some(base + AP_STACK_PAGES as u64 * 4096),
            Err(crate::mm::phys_alloc::AllocError::NoFrames) => {
                SerialPort::puts("[smp] WARN: NoFrames for AP stack cpu=");
                SerialPort::put_u64(cpu_id as u64);
                SerialPort::puts(" attempt=");
                SerialPort::put_u64(attempt as u64);
                SerialPort::puts("/");
                SerialPort::put_u64(AP_STACK_ATTEMPTS as u64);
                SerialPort::puts(" free=");
                SerialPort::put_u64(alloc.free_frames() as u64);
                SerialPort::puts("\n");
                // Brief pause; a concurrent free may coalesce a window.
                core::hint::spin_loop();
            }
            Err(crate::mm::phys_alloc::AllocError::InvalidCount) => {
                SerialPort::puts("[smp] FATAL: invalid AP stack pages (bitmap too small)\n");
                return None;
            }
        }
    }
    SerialPort::puts("[smp] WARN: skipping AP cpu=");
    SerialPort::put_u64(cpu_id as u64);
    SerialPort::puts(" — no contiguous ");
    SerialPort::put_u64(AP_STACK_PAGES as u64);
    SerialPort::puts("-frame window after ");
    SerialPort::put_u64(AP_STACK_ATTEMPTS as u64);
    SerialPort::puts(" attempts\n");
    None
}

pub fn try_allocate_ap_stack(_cpu_id: u32) -> Result<u64, crate::mm::phys_alloc::AllocError> {
    const AP_STACK_PAGES: usize = 17;
    let alloc = crate::mm::heap::get_phys_allocator_mut();
    let base = alloc.try_alloc_contiguous(AP_STACK_PAGES)?;
    Ok(base + AP_STACK_PAGES as u64 * 4096)
}

pub fn smp_snapshot() -> alloc::vec::Vec<(u32, u32, bool, u8, u64, bool, u32, u64)> {
    let mut out = alloc::vec::Vec::new();
    for i in 0..MAX_CPUS {
        let pc = per_cpu_by_id(i as u32);
        if pc.self_ptr.is_null() {
            continue;
        }
        let state = cpu_state(i as u32) as u8;
        let has_task = !pc.current_task.load(Ordering::Relaxed).is_null();
        let preempt = pc.preempt_count.load(Ordering::Relaxed);
        let ticks = pc.sched_ticks.load(Ordering::Relaxed);
        out.push((pc.cpu_id, pc.apic_id, pc.is_bsp, state, pc.stack_top, has_task, preempt, ticks));
    }
    out
}

pub fn cpu_states_snapshot() -> [u8; MAX_CPUS] {
    let mut out = [0u8; MAX_CPUS];
    for i in 0..MAX_CPUS {
        out[i] = CPU_STATES[i].load(Ordering::Relaxed);
    }
    out
}

pub fn ap_ready_snapshot() -> [bool; MAX_CPUS] {
    let mut out = [false; MAX_CPUS];
    for i in 0..MAX_CPUS {
        out[i] = AP_READY[i].ready.load(Ordering::Relaxed);
    }
    out
}
