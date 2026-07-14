//! Interrupt Descriptor Table for x86_64.

use core::sync::atomic::{AtomicPtr, Ordering};

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::platform::x86_64_pc::apic;

static IDT: Once<InterruptDescriptorTable> = Once::new();

// ── Device interrupt dispatch (vectors 33-48) ─────────────────────
//
// Drivers can register a handler for one of the 16 available device
// interrupt vectors. The handler is called with interrupts disabled
// and must not block. EOI is sent automatically after the handler.

const NUM_DEVICE_VECTORS: usize = 16;
const DEVICE_VECTOR_BASE: u8 = 33;
static DEVICE_HANDLERS: [AtomicPtr<fn()>; NUM_DEVICE_VECTORS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NUM_DEVICE_VECTORS];

/// Register a handler for a device interrupt vector (index 0-15, mapping to
/// IDT vectors 33-48). Returns the allocated vector number or `None` if the
/// slot is already taken.
pub fn register_device_handler(handler: fn()) -> Option<u8> {
    for (i, slot) in DEVICE_HANDLERS.iter().enumerate() {
        if slot.load(Ordering::Acquire).is_null() {
            let ptr = handler as *mut fn();
            if slot.compare_exchange(
                core::ptr::null_mut(), ptr,
                Ordering::Release, Ordering::Relaxed,
            ).is_ok() {
                return Some(DEVICE_VECTOR_BASE + i as u8);
            }
        }
    }
    None
}

/// Unregister a previously registered device interrupt handler.
pub fn unregister_device_handler(vector: u8) {
    if vector < DEVICE_VECTOR_BASE || vector >= DEVICE_VECTOR_BASE + NUM_DEVICE_VECTORS as u8 {
        return;
    }
    let idx = (vector - DEVICE_VECTOR_BASE) as usize;
    DEVICE_HANDLERS[idx].store(core::ptr::null_mut(), Ordering::Release);
}

fn device_irq_handler(vector: u8) {
    let idx = (vector - DEVICE_VECTOR_BASE) as usize;
    if idx < NUM_DEVICE_VECTORS {
        let ptr = DEVICE_HANDLERS[idx].load(Ordering::Acquire);
        if !ptr.is_null() {
            let handler: fn() = unsafe { core::mem::transmute(ptr) };
            handler();
        }
    }
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

        // Register APIC timer interrupt at vector 32 (interrupt gate, clears IF).
        idt[32].set_handler_fn(timer_handler).disable_interrupts(true);

        // Register device interrupt vectors 33-48 (interrupt gates, clears IF).
        idt[33].set_handler_fn(irq_33).disable_interrupts(true);
        idt[34].set_handler_fn(irq_34).disable_interrupts(true);
        idt[35].set_handler_fn(irq_35).disable_interrupts(true);
        idt[36].set_handler_fn(irq_36).disable_interrupts(true);
        idt[37].set_handler_fn(irq_37).disable_interrupts(true);
        idt[38].set_handler_fn(irq_38).disable_interrupts(true);
        idt[39].set_handler_fn(irq_39).disable_interrupts(true);
        idt[40].set_handler_fn(irq_40).disable_interrupts(true);
        idt[41].set_handler_fn(irq_41).disable_interrupts(true);
        idt[42].set_handler_fn(irq_42).disable_interrupts(true);
        idt[43].set_handler_fn(irq_43).disable_interrupts(true);
        idt[44].set_handler_fn(irq_44).disable_interrupts(true);
        idt[45].set_handler_fn(irq_45).disable_interrupts(true);
        idt[46].set_handler_fn(irq_46).disable_interrupts(true);
        idt[47].set_handler_fn(irq_47).disable_interrupts(true);
        idt[48].set_handler_fn(irq_48).disable_interrupts(true);

        idt
    });

    idt.load();
}

/// Timer interrupt handler (vector 32).
extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    apic::apic_eoi();
}

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    crate::kerneldump::dump_full_fault(&frame, 0, 0);
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    crate::kerneldump::dump_full_fault(&frame, 0, 3);
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    crate::kerneldump::dump_full_fault(&frame, 0, 6);
}

extern "x86-interrupt" fn invalid_tss_handler(frame: InterruptStackFrame, error_code: u64) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 10);
}

extern "x86-interrupt" fn segment_not_present_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 11);
}

extern "x86-interrupt" fn stack_segment_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 12);
}

extern "x86-interrupt" fn double_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    crate::kerneldump::dump_full_fault(&frame, error_code, 8);
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    crate::kerneldump::dump_full_fault(&frame, error_code.bits(), 14);
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, error_code: u64) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 13);
}

/// Reload the IDT on an Application Processor (IDTR is per-CPU).
///
/// Must be called after the BSP has called `init()` and before the AP
/// enables interrupts.
pub fn init_ap() {
    let idt = IDT.get().expect("IDT not initialised on BSP");
    idt.load();
}
