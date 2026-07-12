use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::mm::phys_alloc::BitmapAllocator;
use crate::arch::Arch;
use crate::arch::CurrentArch;

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
}

/// Max supported CPUs.
pub const MAX_CPUS: usize = 16;

static CPU_COUNT: AtomicU32 = AtomicU32::new(1);

static mut PER_CPU_SLOTS: [PerCpu; MAX_CPUS] = [
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 0, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 1, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 2, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 3, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 4, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 5, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 6, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 7, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 8, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 9, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 10, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 11, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 12, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 13, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 14, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
    PerCpu { self_ptr: core::ptr::null(), cpu_id: 15, apic_id: 0, is_bsp: false, started: AtomicU64::new(0), stack_top: 0, serial_locked: AtomicU64::new(0) },
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

/// Returns `Some(PerCpu)` if `early_init_bsp` has been called, else `None`.
pub fn try_current_per_cpu() -> Option<&'static mut PerCpu> {
    let pc = unsafe { &mut PER_CPU_SLOTS[0] };
    if pc.self_ptr.is_null() {
        None
    } else {
        Some(current_per_cpu())
    }
}

pub fn per_cpu_by_id(cpu_id: u32) -> &'static mut PerCpu {
    assert!((cpu_id as usize) < MAX_CPUS, "per_cpu_by_id: cpu {} out of range", cpu_id);
    unsafe { &mut PER_CPU_SLOTS[cpu_id as usize] }
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
    let pc = unsafe { &mut PER_CPU_SLOTS[0] };
    pc.self_ptr = pc as *const PerCpu;
    pc.cpu_id = 0;
    pc.apic_id = 0;
    pc.is_bsp = true;
    pc.started.store(1, Ordering::Relaxed);

    #[cfg(target_arch = "x86_64")]
    set_gs_base(pc as *const PerCpu as u64);

    #[cfg(target_arch = "riscv64")]
    set_tp(pc as *const PerCpu);
}

#[cfg(target_arch = "x86_64")]
fn set_gs_base(addr: u64) {
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
    unsafe { PER_CPU_SLOTS[0].apic_id = id; }
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
    allocator: &mut BitmapAllocator,
    page_table_root: u64,
    acpi: Option<&crate::acpi::AcpiSubsystem>,
) -> u32 {
    use crate::drivers::serial::SerialPort;
    SerialPort::puts("[smp] init\n");

    let cpus = CurrentArch::discover_cpus(acpi);
    let _bsp_id = cpus.first().map(|(id, _)| *id).unwrap_or(0);

    let mut ap_list = alloc::vec::Vec::new();
    for (cpu_id_offset, &(hardware_id, enabled)) in cpus.iter().enumerate().skip(1) {
        if !enabled {
            continue;
        }
        let cpu_id = cpu_id_offset as u32;
        let stack_top = allocate_ap_stack(allocator, cpu_id);

        let pc = unsafe { &mut PER_CPU_SLOTS[cpu_id as usize] };
        pc.self_ptr = pc as *const PerCpu;
        pc.cpu_id = cpu_id;
        pc.apic_id = hardware_id;
        pc.is_bsp = false;
        pc.started.store(0, Ordering::Relaxed);
        pc.stack_top = stack_top;

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

    let started = unsafe { CurrentArch::wake_aps(allocator, page_table_root, &ap_list) };

    SerialPort::puts("[smp] APs started: ");
    SerialPort::put_u64(started as u64);
    SerialPort::puts("\n");

    total
}

fn allocate_ap_stack(allocator: &mut BitmapAllocator, _cpu_id: u32) -> u64 {
    const AP_STACK_PAGES: usize = 17;
    let base = allocator
        .alloc_contiguous(AP_STACK_PAGES)
        .expect("SMP: OOM for AP stack");
    base + AP_STACK_PAGES as u64 * 4096
}
