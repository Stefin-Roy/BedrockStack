pub trait MsiAllocator: Send + Sync {
    fn allocate_device_vector(&self, handler: fn()) -> Option<u8>;
    fn release_device_vector(&self, vector: u8);
    fn msi_message_address(&self, target_cpu: u32) -> u64;
    fn msi_message_data(&self, vector: u8) -> u16;

    /// Allocate `count` contiguous vectors aligned to `count` when power-of-two
    /// (MSI spec alignment). Returns base vector.
    fn allocate_device_vectors(&self, handler: fn(), count: usize) -> Option<u8> {
        if count == 0 {
            return None;
        }
        if count == 1 {
            return self.allocate_device_vector(handler);
        }
        None
    }
    fn release_device_vectors(&self, base: u8, count: usize) {
        let _ = (base, count);
    }
    fn allocated_vectors(&self) -> usize {
        0
    }
    fn total_vectors(&self) -> usize {
        0
    }
}
