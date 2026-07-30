use super::capability::Capability;
use super::msi::MsiAllocator;

pub struct NullMsi;

impl Capability for NullMsi {
    fn name(&self) -> &str {
        "null-msi"
    }
}

impl MsiAllocator for NullMsi {
    fn allocate_device_vector(&self, _handler: fn()) -> Option<u8> {
        None
    }

    fn release_device_vector(&self, _vector: u8) {}

    fn msi_message_address(&self, _target_cpu: u32) -> u64 {
        0
    }

    fn msi_message_data(&self, _vector: u8) -> u16 {
        0
    }
}

static NULL_MSI: NullMsi = NullMsi;

pub fn init() -> &'static dyn MsiAllocator {
    &NULL_MSI as &'static dyn MsiAllocator
}
