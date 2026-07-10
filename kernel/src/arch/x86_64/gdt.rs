//! Global Descriptor Table + Task State Segment for x86_64 long mode.
//!
//! The TSS provides an Interrupt Stack Table (IST) entry so the double-fault
//! handler always runs on a known-good stack. Without it, a fault that occurs
//! with a corrupt/overflowed stack would escalate straight to a triple fault.

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST slot used by the double-fault handler.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Size of the dedicated double-fault stack (20 KB).
const DF_STACK_SIZE: usize = 4096 * 5;

static mut DF_STACK: [u8; DF_STACK_SIZE] = [0; DF_STACK_SIZE];

static TSS: Once<TaskStateSegment> = Once::new();
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

/// Initialize and load the GDT and TSS.
///
/// # Safety
/// Must be called before IDT init.
pub fn init() {
    let tss = TSS.call_once(|| {
        let mut tss = TaskStateSegment::new();
        let stack_start = VirtAddr::from_ptr(core::ptr::addr_of!(DF_STACK));
        let stack_end = stack_start + DF_STACK_SIZE as u64;
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack_end;
        tss
    });

    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        let code = gdt.add_entry(Descriptor::kernel_code_segment());
        let data = gdt.add_entry(Descriptor::kernel_data_segment());
        let tss = gdt.add_entry(Descriptor::tss_segment(tss));
        (gdt, Selectors { code, data, tss })
    });

    unsafe {
        gdt.load();
        CS::set_reg(selectors.code);
        DS::set_reg(selectors.data);
        ES::set_reg(selectors.data);
        SS::set_reg(selectors.data);
        load_tss(selectors.tss);
    }
}
