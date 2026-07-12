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

/// Size of the dedicated double-fault stack (20 KB).
const DF_STACK_SIZE: usize = 4096 * 5;

/// Per-CPU double-fault stacks.  Each CPU's TSS.IST[0] points into its own
/// slot so that a simultaneous double fault on two CPUs does not corrupt
/// either stack.
static mut DF_STACKS: [[u8; DF_STACK_SIZE]; MAX_CPUS] = [[0; DF_STACK_SIZE]; MAX_CPUS];

/// Per-CPU TSS objects (each CPU gets its own IST stack).
///
/// These must live forever because the GDT descriptor encodes their address.
static mut CPU_TSS: [MaybeUninit<TaskStateSegment>; MAX_CPUS] = [MaybeUninit::uninit(); MAX_CPUS];

/// Per-CPU GDT objects (contains a per-CPU TSS entry).
///
/// The GDT heap-buffer stays alive because the struct is stored here.
static mut CPU_GDT: [MaybeUninit<GlobalDescriptorTable>; MAX_CPUS] = [const { MaybeUninit::uninit() }; MAX_CPUS];

/// Return the kernel GDT pointer (base + limit) for AP trampoline use.
///
/// Reads the currently loaded GDTR — must be called after `init()`.
pub fn get_gdt_ptr() -> (u64, u16) {
    unsafe {
        use x86_64::instructions::tables::sgdt;
        let desc = sgdt();
        (desc.base.as_u64(), desc.limit)
    }
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
        let df_stack = unsafe { &DF_STACKS[cpu_id] };
        VirtAddr::from_ptr(df_stack.as_ptr()) + DF_STACK_SIZE as u64
    };

    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack_end;

    // Store TSS at a stable address *before* creating the GDT descriptor.
    unsafe { CPU_TSS[cpu_id].write(tss); }
    let tss_ref = unsafe { &*CPU_TSS[cpu_id].as_ptr() };

    // ── build per-CPU GDT ───────────────────────────────────────────
    let mut gdt = GlobalDescriptorTable::new();
    let code_sel = gdt.append(Descriptor::kernel_code_segment());
    let data_sel = gdt.append(Descriptor::kernel_data_segment());
    let tss_sel = gdt.append(Descriptor::tss_segment(tss_ref));

    unsafe {
        CPU_GDT[cpu_id].write(gdt);

        // Load the GDT, segments, and task register for this CPU.
        let gdt_ref = &*CPU_GDT[cpu_id].as_ptr();
        gdt_ref.load();
        CS::set_reg(code_sel);
        DS::set_reg(data_sel);
        ES::set_reg(data_sel);
        SS::set_reg(data_sel);
        load_tss(tss_sel);
    }
}
