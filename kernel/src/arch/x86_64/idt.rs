//! Interrupt Descriptor Table for x86_64.

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::arch::{Arch, CurrentArch};
use crate::drivers::serial::SerialPort;
use crate::platform::x86_64_pc::apic;

static IDT: Once<InterruptDescriptorTable> = Once::new();

// ── Device interrupt stubs (vectors 33-48) ─────────────────────────

fn device_irq_handler(_vector: u8) {
    // Future: dispatch to registered driver handlers
    apic::apic_eoi();
}

extern "x86-interrupt" fn irq_33(_sf: InterruptStackFrame) { device_irq_handler(33); }
extern "x86-interrupt" fn irq_34(_sf: InterruptStackFrame) { device_irq_handler(34); }
extern "x86-interrupt" fn irq_35(_sf: InterruptStackFrame) { device_irq_handler(35); }
extern "x86-interrupt" fn irq_36(_sf: InterruptStackFrame) { device_irq_handler(36); }
extern "x86-interrupt" fn irq_37(_sf: InterruptStackFrame) { device_irq_handler(37); }
extern "x86-interrupt" fn irq_38(_sf: InterruptStackFrame) { device_irq_handler(38); }
extern "x86-interrupt" fn irq_39(_sf: InterruptStackFrame) { device_irq_handler(39); }
extern "x86-interrupt" fn irq_40(_sf: InterruptStackFrame) { device_irq_handler(40); }
extern "x86-interrupt" fn irq_41(_sf: InterruptStackFrame) { device_irq_handler(41); }
extern "x86-interrupt" fn irq_42(_sf: InterruptStackFrame) { device_irq_handler(42); }
extern "x86-interrupt" fn irq_43(_sf: InterruptStackFrame) { device_irq_handler(43); }
extern "x86-interrupt" fn irq_44(_sf: InterruptStackFrame) { device_irq_handler(44); }
extern "x86-interrupt" fn irq_45(_sf: InterruptStackFrame) { device_irq_handler(45); }
extern "x86-interrupt" fn irq_46(_sf: InterruptStackFrame) { device_irq_handler(46); }
extern "x86-interrupt" fn irq_47(_sf: InterruptStackFrame) { device_irq_handler(47); }
extern "x86-interrupt" fn irq_48(_sf: InterruptStackFrame) { device_irq_handler(48); }

/// Initialize and load the IDT.
///
/// # Safety
/// Must be called after GDT init (the double-fault handler relies on the IST
/// entry configured there).
pub fn init() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();

        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.invalid_tss.set_handler_fn(invalid_tss_handler);
        idt.segment_not_present
            .set_handler_fn(segment_not_present_handler);
        idt.stack_segment_fault
            .set_handler_fn(stack_segment_fault_handler);
        idt.general_protection_fault.set_handler_fn(gpf_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);

        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(crate::arch::x86_64::gdt::DOUBLE_FAULT_IST_INDEX);
        }

        // Register APIC timer interrupt at vector 32.
        idt[32].set_handler_fn(timer_handler);

        // Register device interrupt vectors 33-48.
        idt[33].set_handler_fn(irq_33);
        idt[34].set_handler_fn(irq_34);
        idt[35].set_handler_fn(irq_35);
        idt[36].set_handler_fn(irq_36);
        idt[37].set_handler_fn(irq_37);
        idt[38].set_handler_fn(irq_38);
        idt[39].set_handler_fn(irq_39);
        idt[40].set_handler_fn(irq_40);
        idt[41].set_handler_fn(irq_41);
        idt[42].set_handler_fn(irq_42);
        idt[43].set_handler_fn(irq_43);
        idt[44].set_handler_fn(irq_44);
        idt[45].set_handler_fn(irq_45);
        idt[46].set_handler_fn(irq_46);
        idt[47].set_handler_fn(irq_47);
        idt[48].set_handler_fn(irq_48);

        idt
    });

    idt.load();
}

/// Timer interrupt handler (vector 32).
extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    apic::apic_eoi();
}

/// Print a short message then halt forever.
fn fault_halt(name: &str) -> ! {
    SerialPort::puts("\n*** CPU EXCEPTION: ");
    SerialPort::puts(name);
    SerialPort::puts("\n");
    loop {
        CurrentArch::disable_interrupts();
        CurrentArch::halt();
    }
}

extern "x86-interrupt" fn divide_error_handler(_stack_frame: InterruptStackFrame) {
    fault_halt("divide error");
}

extern "x86-interrupt" fn breakpoint_handler(_stack_frame: InterruptStackFrame) {
    SerialPort::puts("\n*** breakpoint\n");
}

extern "x86-interrupt" fn invalid_opcode_handler(_stack_frame: InterruptStackFrame) {
    fault_halt("invalid opcode");
}

extern "x86-interrupt" fn invalid_tss_handler(_stack_frame: InterruptStackFrame, _error_code: u64) {
    fault_halt("invalid TSS");
}

extern "x86-interrupt" fn segment_not_present_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: u64,
) {
    fault_halt("segment not present");
}

extern "x86-interrupt" fn stack_segment_fault_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: u64,
) {
    fault_halt("stack-segment fault");
}

extern "x86-interrupt" fn double_fault_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    fault_halt("double fault");
}

extern "x86-interrupt" fn page_fault_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: PageFaultErrorCode,
) {
    fault_halt("page fault");
}

extern "x86-interrupt" fn gpf_handler(_stack_frame: InterruptStackFrame, _error_code: u64) {
    fault_halt("general protection fault");
}
