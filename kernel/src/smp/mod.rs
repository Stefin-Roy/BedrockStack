use crate::services::KernelServices;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

/// SMP initialization guard — prevents double-init which would double-start APs,
/// leak stacks, and corrupt the CPU counter.
static SMP_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Per-CPU online state for future hotplug support.
///
/// 0 = Offline, 1 = Starting, 2 = Online.
/// The BSP transitions 0→2 in `early_init_bsp`; APs transition 0→1 in
/// `smp::init` then 1→2 in their respective `ap_entry`.
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
}

impl From<u8> for CpuState {
    fn from(v: u8) -> Self {
        match v {
            1 => CpuState::Starting,
            2 => CpuState::Online,
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
    };
    CPU_STATES[cpu_id as usize].store(new_val, Ordering::Release);
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
    /// Raw pointer to the currently running task (opaque to the smp layer).
    pub current_task: *mut core::ffi::c_void,
    /// Set when a reschedule is needed (tick or wake). Checked on IRQ/syscall return.
    pub need_resched: AtomicBool,
    /// Nesting count for preemptive-critical sections. While >0 tick does not preempt.
    pub preempt_count: AtomicU32,
    /// Per-CPU tick counter for scheduler quantum tracking.
    pub sched_ticks: AtomicU64,
    /// True when this CPU's scheduler has been initialized (after task::init).
    pub sched_active: AtomicBool,
}

/// Byte offset of `PerCpu::syscall_rsp0` within the struct, for the syscall
/// entry asm (`gs:[PERCPU_SYSCALL_RSP0_OFF]`).
pub const PERCPU_SYSCALL_RSP0_OFF: u64 = core::mem::offset_of!(PerCpu, syscall_rsp0) as u64;

/// Max supported CPUs.
pub const MAX_CPUS: usize = 16;

static CPU_COUNT: AtomicU32 = AtomicU32::new(1);

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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
        current_task: core::ptr::null_mut(),
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
    pc.current_task = core::ptr::null_mut();
    pc.need_resched.store(false, Ordering::Relaxed);
    pc.preempt_count.store(0, Ordering::Relaxed);
    pc.sched_ticks.store(0, Ordering::Relaxed);
    pc.sched_active.store(false, Ordering::Relaxed);

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

    let cpus = services.cpu.discover_cpus(acpi);
    let _bsp_id = cpus.first().map(|(id, _)| *id).unwrap_or(0);

    let mut ap_list = alloc::vec::Vec::new();
    for (cpu_id_offset, &(hardware_id, enabled)) in cpus.iter().enumerate().skip(1) {
        if !enabled {
            continue;
        }
        let cpu_id = cpu_id_offset as u32;
        let stack_top = allocate_ap_stack(cpu_id);

        set_cpu_state(cpu_id, CpuState::Starting);

        let pc = slot_mut(cpu_id as usize);
        pc.self_ptr = pc as *const PerCpu;
        pc.cpu_id = cpu_id;
        pc.apic_id = hardware_id;
        pc.is_bsp = false;
        pc.started.store(0, Ordering::Relaxed);
        pc.stack_top = stack_top;
        pc.syscall_rsp0 = 0;
        pc.current_task = core::ptr::null_mut();
        pc.need_resched.store(false, Ordering::Relaxed);
        pc.preempt_count.store(0, Ordering::Relaxed);
        pc.sched_ticks.store(0, Ordering::Relaxed);
        pc.sched_active.store(false, Ordering::Relaxed);
        slot_release(cpu_id as usize);

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
    let total = 1 + ap_count as u32;
    CPU_COUNT.store(total, Ordering::Relaxed);

    SerialPort::puts("[smp] total CPUs: ");
    SerialPort::put_u64(total as u64);
    SerialPort::puts("\n");

    if ap_count == 0 {
        SerialPort::puts("[smp] no APs found, running uniprocessor\n");
        return 1;
    }

    let started = unsafe { services.cpu.wake_aps(page_table_root, &ap_list) };

    SerialPort::puts("[smp] APs started: ");
    SerialPort::put_u64(started as u64);
    SerialPort::puts("\n");

    total
}

fn allocate_ap_stack(_cpu_id: u32) -> u64 {
    const AP_STACK_PAGES: usize = 17;
    let base = crate::mm::heap::get_phys_allocator_mut()
        .alloc_contiguous(AP_STACK_PAGES)
        .expect("SMP: OOM for AP stack");
    base + AP_STACK_PAGES as u64 * 4096
}
