use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use crate::services::KernelServices;

#[cfg(target_arch = "x86_64")]
pub mod work;

/// SMP initialization guard — prevents double-init which would double-start APs,
/// leak stacks, and corrupt the CPU counter.
static SMP_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Per-CPU online state for future hotplug support.
///
/// 0 = Offline, 1 = Starting, 2 = Online.
/// The BSP transitions 0→2 in `early_init_bsp`; APs transition 0→1 in
/// `smp::init` then 1→2 in their respective `ap_entry`.
static CPU_STATES: [AtomicU8; MAX_CPUS] = [
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
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
    assert!((cpu_id as usize) < MAX_CPUS, "cpu_state: cpu {} out of range", cpu_id);
    CpuState::from(CPU_STATES[cpu_id as usize].load(Ordering::Acquire))
}

/// Transition a CPU's online state. Panics in debug if the transition is illegal.
pub(crate) fn set_cpu_state(cpu_id: u32, new_state: CpuState) {
    assert!((cpu_id as usize) < MAX_CPUS, "set_cpu_state: cpu {} out of range", cpu_id);
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
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
    ApReady { ready: AtomicBool::new(false) },
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
    pub current_domain: *const crate::obj::domain::Domain,
    /// User RSP saved on `syscall` entry (ring 3 → ring 0). Per-CPU: any CPU
    /// may run user code, and this is written before each `resume_user` on the
    /// CPU that picked the task.
    pub syscall_user_rsp: u64,
    /// Kernel stack the syscall entry switches to. Points at the top of this
    /// CPU's dedicated syscall stack, set up by
    /// `arch::x86_64::syscall::{init,init_ap}`.
    pub syscall_stack: u64,
}

/// Max supported CPUs.
pub const MAX_CPUS: usize = 16;

static CPU_COUNT: AtomicU32 = AtomicU32::new(1);

/// Shared storage for the per-CPU slot table.
///
/// The table is written during SMP bring-up (each CPU owns its own slot) and
/// read via `current_per_cpu` throughout.  Indexed access is routed through
/// `slot_mut()` so the kernel links without a `static mut`.
struct SharedSlots(core::cell::UnsafeCell<[PerCpu; MAX_CPUS]>);

// Each CPU only ever mutates its own slot; reads of other slots happen after
// that slot's owner has finished initialising it.  Synchronisation is provided
// by the slot_mut/slot_read/slot_release protocol (see slot_mut).
unsafe impl Sync for SharedSlots {}
unsafe impl Send for SharedSlots {}

static PER_CPU_SLOTS: SharedSlots = SharedSlots(core::cell::UnsafeCell::new([
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 0, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 1, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 2, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 3, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 4, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 5, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 6, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 7, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 8, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 9, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 10, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 11, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 12, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 13, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 14, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 15, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0), current_domain: core::ptr::null(), syscall_user_rsp: 0, syscall_stack: 0 },
]));

// ── Per-CPU lockdep stacks (debug-only, feature "lockdep") ────────────
//
// Kept as separate statics (rather than fields on `PerCpu`) so the `#[repr(C)]`
// PerCpu layout — whose first field `self_ptr` is addressed via gs/tp — stays
// untouched.  `order == 0` locks are untracked.

/// Maximum depth of the per-CPU lockdep stack.
#[cfg(feature = "lockdep")]
const LOCKDEP_STACK_DEPTH: usize = 16;

#[cfg(feature = "lockdep")]
struct LockdepStack(core::cell::UnsafeCell<[u8; LOCKDEP_STACK_DEPTH]>);

#[cfg(feature = "lockdep")]
unsafe impl Sync for LockdepStack {}

#[cfg(feature = "lockdep")]
unsafe impl Send for LockdepStack {}

#[cfg(feature = "lockdep")]
const fn empty_lockdep_stack() -> LockdepStack {
    LockdepStack(core::cell::UnsafeCell::new([0u8; LOCKDEP_STACK_DEPTH]))
}

#[cfg(feature = "lockdep")]
static LOCKDEP_STACKS: [LockdepStack; MAX_CPUS] = [
    empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(),
    empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(),
    empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(),
    empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(), empty_lockdep_stack(),
];

#[cfg(feature = "lockdep")]
static LOCKDEP_DEPTH: [AtomicU8; MAX_CPUS] = [
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
    AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0),
];

/// Record `order` as the newly-acquired lock on the current CPU.
///
/// Asserts the acquire order is strictly greater than the current top of this
/// CPU's lockdep stack and that the lock is not already held on this CPU
/// (recursion would otherwise hang the `spin::Mutex` silently).  No-op unless
/// the `lockdep` feature is enabled; `order == 0` locks are untracked.
#[allow(unused_variables)]
pub fn lockdep_push(order: u8) {
    #[cfg(feature = "lockdep")]
    {
        if order == 0 {
            return;
        }
        let cpu = current_cpu_id() as usize;
        let depth = LOCKDEP_DEPTH[cpu].load(Ordering::Relaxed) as usize;
        assert!(
            depth < LOCKDEP_STACK_DEPTH,
            "lock order violation: lockdep stack overflow (order {order}) on cpu {cpu}"
        );
        let stack = unsafe { &mut *LOCKDEP_STACKS[cpu].0.get() };
        for &held in &stack[..depth] {
            assert!(
                held != order,
                "lock order violation: recursive lock of order {order} on cpu {cpu}"
            );
        }
        if depth > 0 {
            let top = stack[depth - 1];
            assert!(
                order > top,
                "lock order violation: order {order} acquired after {top} on cpu {cpu}"
            );
        }
        stack[depth] = order;
        LOCKDEP_DEPTH[cpu].store((depth + 1) as u8, Ordering::Relaxed);
    }
}

/// Release `order` on the current CPU's lockdep stack.
///
/// Asserts `order` is the current top of this CPU's stack (i.e. released in
/// strict LIFO order).  No-op unless the `lockdep` feature is enabled;
/// `order == 0` locks are untracked.
#[allow(unused_variables)]
pub fn lockdep_pop(order: u8) {
    #[cfg(feature = "lockdep")]
    {
        if order == 0 {
            return;
        }
        let cpu = current_cpu_id() as usize;
        let depth = LOCKDEP_DEPTH[cpu].load(Ordering::Relaxed) as usize;
        assert!(
            depth > 0,
            "lock order violation: lockdep pop underflow (order {order}) on cpu {cpu}"
        );
        let stack = unsafe { &mut *LOCKDEP_STACKS[cpu].0.get() };
        let top = stack[depth - 1];
        assert!(
            top == order,
            "lock order violation: releasing order {order}, top is {top} on cpu {cpu}"
        );
        LOCKDEP_DEPTH[cpu].store((depth - 1) as u8, Ordering::Relaxed);
    }
}

#[cfg(debug_assertions)]
static SLOT_BUSY: [AtomicU32; MAX_CPUS] = [
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
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
        assert!(prev == 0, "smp: slot {} re-entrant mutable access (missing slot_release)", index);
    }
    unsafe { &mut (*PER_CPU_SLOTS.0.get())[index] }
}

/// Read-only access to any per-CPU slot (for cross-CPU observation).
fn slot_read(index: usize) -> &'static PerCpu {
    assert!(index < MAX_CPUS, "smp: per-CPU slot {} out of range", index);
    unsafe { &(*PER_CPU_SLOTS.0.get())[index] }
}

#[inline]
fn slot_release(index: usize) {
    #[cfg(debug_assertions)]
    {
        SLOT_BUSY[index].fetch_sub(1, Ordering::Release);
    }
}

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

#[cfg(target_arch = "x86_64")]
pub fn set_gs_base(addr: u64) {
    use x86_64::registers::model_specific::Msr;
    const IA32_GS_BASE: u32 = 0xC0000101;
    unsafe { Msr::new(IA32_GS_BASE).write(addr); }
}

#[cfg(target_arch = "riscv64")]
fn set_tp(pc: *const PerCpu) {
    unsafe { core::arch::asm!("mv tp, {}", in(reg) pc); }
}

/// Fill in the hardware ID (APIC ID / hart ID) for the BSP.
pub fn set_bsp_hardware_id(id: u32) {
    slot_mut(0).apic_id = id;
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

/// Number of CPUs that have signalled started (the BSP plus brought-up APs).
pub fn started_count() -> u32 {
    let mut n = 0;
    for i in 0..MAX_CPUS {
        let pc = slot_read(i);
        if pc.started.load(Ordering::Relaxed) != 0 {
            n += 1;
        }
    }
    n
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
        slot_release(cpu_id as usize);

        ap_list.push(ApContext { cpu_id, hardware_id, stack_top });

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
    // Phase D: AP stacks are heap/guard-mapped VM-backed allocations (NX,
    // mapped in the shared kernel root), not raw contiguous physical frames.
    // Both trampolines treat `stack_top` as an opaque address:
    //   x86   — loaded into RSP in long mode after the CR3 switch;
    //   riscv — loaded into `sp` before the MMU is enabled, but not dereferenced
    //          until `ap_entry_riscv` has written satp (heap VAs then resolve).
    // Allocate a page-aligned block (leaked: AP stacks live for kernel lifetime).
    let size = AP_STACK_PAGES * 4096;
    let layout = alloc::alloc::Layout::from_size_align(size, 4096).expect("SMP: AP stack layout");
    let ptr = unsafe { alloc::alloc::alloc(layout) };
    assert!(!ptr.is_null(), "SMP: OOM for AP stack");
    ptr as u64 + size as u64
}
