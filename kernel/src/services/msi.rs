
pub trait MsiAllocator: Send + Sync {
    fn allocate_device_vector(&self, handler: fn()) -> Option<u8>;
    fn release_device_vector(&self, vector: u8);
    fn msi_message_address(&self, target_cpu: u32) -> u64;
    fn msi_message_data(&self, vector: u8) -> u16;
}
