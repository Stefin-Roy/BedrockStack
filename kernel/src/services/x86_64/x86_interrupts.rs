use super::super::interrupts::InterruptManager;
use crate::platform::x86_64_pc::{apic, ioapic};

pub struct X86Interrupts;

impl InterruptManager for X86Interrupts {
    fn register_handler(&self, vector: u8, handler: fn()) {
        let idx = (vector - crate::arch::x86_64::idt::DEVICE_VECTOR_BASE) as usize;
        if idx < crate::arch::x86_64::idt::NUM_DEVICE_VECTORS {
            crate::arch::x86_64::idt::register_device_handler_at(vector, handler);
        }
    }

    fn unregister_handler(&self, vector: u8) {
        crate::arch::x86_64::idt::unregister_device_handler(vector);
    }

    fn enable(&self, vector: u8) {
        ioapic::unmask_irq(vector as u32);
    }

    fn disable(&self, vector: u8) {
        ioapic::mask_irq(vector as u32);
    }

    fn eoi(&self) {
        apic::apic_eoi();
    }
}

static X86_INTERRUPTS: X86Interrupts = X86Interrupts;

pub fn init() -> &'static dyn InterruptManager {
    &X86_INTERRUPTS as &'static dyn InterruptManager
}
