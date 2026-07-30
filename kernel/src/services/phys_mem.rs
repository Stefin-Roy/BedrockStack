use super::capability::Capability;

pub trait PhysicalMemoryAllocator: Capability {
    fn alloc_frames(&mut self, count: usize) -> Result<u64, ()>;
    fn free_frames(&mut self, addr: u64, count: usize);
    fn reserve_region(&mut self, start: u64, end: u64);
    fn total_frames(&self) -> usize;
}
