//! Global Descriptor Table + Task State Segment for x86_64 long mode.
//!
//! The TSS provides an Interrupt Stack Table (IST) entry so the double-fault
//! handler always runs on a known-good stack. Without it, a fault that occurs
//! with a corrupt/overflowed stack would escalate straight to a triple fault.

use core::mem::MaybeUninit;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use crate::smp::{MAX_CPUS, current_cpu_id};

/// IST slot used by the double-fault handler.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Kernel code segment selector (ring 0).
pub const KERNEL_CS_SELECTOR: u16 = 0x08;
/// Kernel data segment selector (ring 0).
pub const KERNEL_DS_SELECTOR: u16 = 0x10;
/// User data segment selector (ring 3) — GDT index 3, RPL 3.
pub const USER_DS_SELECTOR: u16 = 0x1B;
/// User code segment selector (ring 3) — GDT index 4, RPL 3.
pub const USER_CS_SELECTOR: u16 = 0x23;

/// Size of the dedicated double-fault stack (20 KB).
const DF_STACK_SIZE: usize = 4096 * 5;

/// Shared storage wrapper so the per-CPU GDT/TSS/DF tables link without a
/// `static mut`.  Each CPU only ever touches its own slot (indexed by its
/// `cpu_id`), so concurrent access is to disjoint slots.
struct Shared<T>(core::cell::UnsafeCell<T>);

unsafe impl Sync for Shared<[[u8; DF_STACK_SIZE]; MAX_CPUS]> {}
unsafe impl Sync for Shared<[MaybeUninit<TaskStateSegment>; MAX_CPUS]> {}
unsafe impl Sync for Shared<[MaybeUninit<GlobalDescriptorTable>; MAX_CPUS]> {}
unsafe impl<T: Send> Send for Shared<T> {}

/// Per-CPU double-fault stacks.  Each CPU's TSS.IST[0] points into its own
/// slot so that a simultaneous double fault on two CPUs does not corrupt
/// either stack.
static DF_STACKS: Shared<[[u8; DF_STACK_SIZE]; MAX_CPUS]> =
    Shared(core::cell::UnsafeCell::new([[0; DF_STACK_SIZE]; MAX_CPUS]));

/// Per-CPU TSS objects (each CPU gets its own IST stack).
///
/// These must live forever because the GDT descriptor encodes their address.
static CPU_TSS: Shared<[MaybeUninit<TaskStateSegment>; MAX_CPUS]> =
    Shared(core::cell::UnsafeCell::new([MaybeUninit::uninit(); MAX_CPUS]));

/// Per-CPU GDT objects (contains a per-CPU TSS entry).
///
/// The GDT heap-buffer stays alive because the struct is stored here.
static CPU_GDT: Shared<[MaybeUninit<GlobalDescriptorTable>; MAX_CPUS]> =
    Shared(core::cell::UnsafeCell::new([const { MaybeUninit::uninit() }; MAX_CPUS]));

fn df_stacks() -> &'static mut [[u8; DF_STACK_SIZE]; MAX_CPUS] {
    unsafe { &mut *DF_STACKS.0.get() }
}

fn cpu_tss() -> &'static mut [MaybeUninit<TaskStateSegment>; MAX_CPUS] {
    unsafe { &mut *CPU_TSS.0.get() }
}

fn cpu_gdt() -> &'static mut [MaybeUninit<GlobalDescriptorTable>; MAX_CPUS] {
    unsafe { &mut *CPU_GDT.0.get() }
}

/// Return the kernel GDT pointer (base + limit) for AP trampoline use.
///
/// Reads the currently loaded GDTR — must be called after `init()`.
pub fn get_gdt_ptr() -> (u64, u16) {
    use x86_64::instructions::tables::sgdt;
    let desc = sgdt();
    (desc.base.as_u64(), desc.limit)
}

/// Initialize and load the GDT and TSS for the *current* CPU.
///
/// Each CPU gets its own TSS (and thus its own double-fault IST stack).
/// Must be called once per CPU before any interrupts are enabled.
pub fn init() {
    let cpu_id = current_cpu_id() as usize;
    assert!(cpu_id < MAX_CPUS, "GDT: CPU {} out of range", cpu_id);

    // ── build per-CPU TSS ───────────────────────────────────────────
    let stack_end = {
        let df_stack = &df_stacks()[cpu_id];
        VirtAddr::from_ptr(df_stack.as_ptr()) + DF_STACK_SIZE as u64
    };

    // RSP0 — the kernel stack the CPU switches to when an interrupt or
    // exception arrives from ring 3. The BSP uses the top of the kernel's
    // high `.stack`; APs use their own AP stack. Needed for user-mode
    // faults/interrupts to have a valid kernel stack.
    let rsp0 = {
        let ap_top = crate::smp::per_cpu_by_id(cpu_id as u32).stack_top;
        if ap_top != 0 {
            VirtAddr::new(ap_top)
        } else {
            let stack_end = unsafe { &crate::__stack_end as *const u8 };
            VirtAddr::from_ptr(stack_end)
        }
    };

    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack_end;
    tss.privilege_stack_table[0] = rsp0;

    // Store TSS at a stable address *before* creating the GDT descriptor.
    cpu_tss()[cpu_id].write(tss);
    let tss_ref = unsafe { &*cpu_tss()[cpu_id].as_ptr() };

    // ── build per-CPU GDT ───────────────────────────────────────────
    let mut gdt = GlobalDescriptorTable::new();
    let code_sel = gdt.append(Descriptor::kernel_code_segment());
    let data_sel = gdt.append(Descriptor::kernel_data_segment());
    let _user_data_sel = gdt.append(Descriptor::user_data_segment());
    let _user_code_sel = gdt.append(Descriptor::user_code_segment());
    let tss_sel = gdt.append(Descriptor::tss_segment(tss_ref));

    unsafe {
        cpu_gdt()[cpu_id].write(gdt);

        // Load the GDT, segments, and task register for this CPU.
        let gdt_ref = &*cpu_gdt()[cpu_id].as_ptr();
        gdt_ref.load();
        CS::set_reg(code_sel);
        DS::set_reg(data_sel);
        ES::set_reg(data_sel);
        SS::set_reg(data_sel);
        load_tss(tss_sel);
    }
}
