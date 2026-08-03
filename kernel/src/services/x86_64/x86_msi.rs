use crate::arch::x86_64::idt;

use super::super::msi::MsiAllocator;

pub struct X86Msi;

impl MsiAllocator for X86Msi {
    fn allocate_device_vector(&self, handler: fn()) -> Option<u8> {
        idt::register_device_handler(handler)
    }

    fn release_device_vector(&self, vector: u8) {
        idt::unregister_device_handler(vector);
    }

    fn msi_message_address(&self, target_cpu: u32) -> u64 {
        0xFEE00000 | ((target_cpu as u64) << 12)
    }

    fn msi_message_data(&self, vector: u8) -> u16 {
        vector as u16
    }
}

static X86_MSI: X86Msi = X86Msi;

pub fn init() -> &'static dyn MsiAllocator {
    &X86_MSI as &'static dyn MsiAllocator
}
