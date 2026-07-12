//! Interrupt Descriptor Table for x86_64.

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::arch::{Arch, CurrentArch};
use crate::drivers::serial::SerialPort;
use crate::platform::x86_64_pc::apic;

static IDT: Once<InterruptDescriptorTable> = Once::new();

// ── Device interrupt stubs (vectors 33-48) ─────────────────────────

fn device_irq_handler(_vector: u8) {
    apic::apic_eoi();
}

macro_rules! irq_stub {
    ($n:literal) => {
        extern "x86-interrupt" fn irq_$n(_sf: InterruptStackFrame) { device_irq_handler($n); }
    };
}

irq_stub!(33); irq_stub!(34); irq_stub!(35); irq_stub!(36);
irq_stub!(37); irq_stub!(38); irq_stub!(39); irq_stub!(40);
irq_stub!(41); irq_stub!(42); irq_stub!(43); irq_stub!(44);
irq_stub!(45); irq_stub!(46); irq_stub!(47); irq_stub!(48);

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
        macro_rules! set_irq {
            ($n:literal) => { idt[$n].set_handler_fn(irq_$n); };
        }
        set_irq!(33); set_irq!(34); set_irq!(35); set_irq!(36);
        set_irq!(37); set_irq!(38); set_irq!(39); set_irq!(40);
        set_irq!(41); set_irq!(42); set_irq!(43); set_irq!(44);
        set_irq!(45); set_irq!(46); set_irq!(47); set_irq!(48);

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
