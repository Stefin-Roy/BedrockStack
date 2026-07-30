use crate::mm::vmm::PageFlags;

use super::capability::Capability;

pub trait VirtualMemoryManager: Capability {
    fn map(&mut self, vaddr: u64, paddr: u64, size: u64, flags: PageFlags);
    fn unmap(&mut self, vaddr: u64, size: u64);
    fn translate(&self, vaddr: u64) -> Option<u64>;
    fn root(&self) -> u64;
    fn flush_tlb(&self);
}
