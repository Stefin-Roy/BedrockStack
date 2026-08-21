//! Global Descriptor Table + Task State Segment for x86_64 long mode.
//!
//! The TSS provides an Interrupt Stack Table (IST) entry so the double-fault
//! handler always runs on a known-good stack. Without it, a fault that occurs
//! with a corrupt/overflowed stack would escalate straight to a triple fault.

use core::mem::MaybeUninit;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::segmentation::SegmentSelector;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
use x86_64::structures::tss::TaskStateSegment;

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
static mut CPU_GDT: [MaybeUninit<GlobalDescriptorTable>; MAX_CPUS] =
    [const { MaybeUninit::uninit() }; MAX_CPUS];

/// Selector for the user code segment (0x28, DPL3). Written once by the first
/// CPU through `init()` — always the BSP, since it runs before any AP is woken.
/// The GDT layout is identical on every CPU, so one value suffices.
static USER_CODE_SEL: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
static USER_DATA_SEL: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
static SYSCALL_KERNEL_CS: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);

/// User code selector (0x28). Valid after BSP `init()`.
pub fn user_code_sel() -> SegmentSelector {
    SegmentSelector(USER_CODE_SEL.load(core::sync::atomic::Ordering::Relaxed))
}

/// User data selector (0x20). Valid after BSP `init()`.
pub fn user_data_sel() -> SegmentSelector {
    SegmentSelector(USER_DATA_SEL.load(core::sync::atomic::Ordering::Relaxed))
}

/// Kernel CS that SYSCALL lands in (0x18). Valid after BSP `init()`.
pub fn syscall_kernel_cs() -> SegmentSelector {
    SegmentSelector(SYSCALL_KERNEL_CS.load(core::sync::atomic::Ordering::Relaxed))
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
        let df_stack = unsafe { &DF_STACKS[cpu_id] };
        VirtAddr::from_ptr(df_stack.as_ptr()) + DF_STACK_SIZE as u64
    };

    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack_end;

    // Store TSS at a stable address *before* creating the GDT descriptor.
    unsafe {
        CPU_TSS[cpu_id].write(tss);
    }
    let tss_ref = unsafe { &*CPU_TSS[cpu_id].as_ptr() };

    // ── build per-CPU GDT ───────────────────────────────────────────
    let mut gdt = GlobalDescriptorTable::new();
    let code_sel = gdt.append(Descriptor::kernel_code_segment()); // 0x08
    let data_sel = gdt.append(Descriptor::kernel_data_segment()); // 0x10
    let syscall_kernel_cs = gdt.append(Descriptor::kernel_code_segment()); // 0x18 (SYSCALL landing CS)
    let user_data_sel = gdt.append(Descriptor::user_data_segment()); // 0x20
    let user_code_sel = gdt.append(Descriptor::user_code_segment()); // 0x28
    let tss_sel = gdt.append(Descriptor::tss_segment(tss_ref)); // 0x30

    unsafe {
        CPU_GDT[cpu_id].write(gdt);

        // The GDT layout is identical on every CPU, so these selectors are the
        // same everywhere. The BSP writes them first (no AP is woken yet);
        // later AP writes are idempotent. Use atomics to avoid static mut UB.
        SYSCALL_KERNEL_CS.store(syscall_kernel_cs.0, core::sync::atomic::Ordering::Release);
        USER_DATA_SEL.store(user_data_sel.0, core::sync::atomic::Ordering::Release);
        USER_CODE_SEL.store(user_code_sel.0, core::sync::atomic::Ordering::Release);

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

/// Update the current CPU's TSS.rsp0 (kernel stack top used on interrupt/
/// syscall entry). Must be called after `init()` on that CPU. No TR reload is
/// needed — the TSS descriptor base is unchanged, only the struct field moves.
/// IST0 (double-fault stack) is left untouched.
pub fn set_kernel_stack(top: u64) {
    let cpu = current_cpu_id() as usize;
    let tss = unsafe { &mut *CPU_TSS[cpu].as_mut_ptr() };
    tss.privilege_stack_table[0] = VirtAddr::new(top);
}
