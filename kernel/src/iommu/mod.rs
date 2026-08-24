//! IOMMU subsystem — Intel VT-d (DMAR) backend.
//!
//! Gated to x86_64; RISC-V builds get an empty stub.

#[cfg(target_arch = "x86_64")]
pub mod dma_remap;
#[cfg(target_arch = "x86_64")]
pub mod qi;
#[cfg(target_arch = "x86_64")]
pub mod slpt;
#[cfg(target_arch = "x86_64")]
pub mod vt_d;

#[cfg(target_arch = "x86_64")]
pub use dma_remap::IommuDma;
#[cfg(target_arch = "x86_64")]
pub use vt_d::{fault_handler, init, is_enabled, is_present, program_fault_msi};

#[cfg(target_arch = "x86_64")]
pub fn update_alloc(alloc: *mut crate::mm::phys_alloc::BitmapAllocator) {
    dma_remap::update_alloc(alloc);
}

#[cfg(not(target_arch = "x86_64"))]
pub fn is_enabled() -> bool {
    false
}
#[cfg(not(target_arch = "x86_64"))]
pub fn is_present() -> bool {
    false
}
#[cfg(not(target_arch = "x86_64"))]
pub fn fault_handler() {}
#[cfg(not(target_arch = "x86_64"))]
pub fn init(
    _dmar: &crate::acpi::DmarInfo,
    _root: u64,
    _alloc: *mut crate::mm::phys_alloc::BitmapAllocator,
) -> bool {
    false
}
#[cfg(not(target_arch = "x86_64"))]
pub fn program_fault_msi(_vector: u8, _apic_id: u32) {}
#[cfg(not(target_arch = "x86_64"))]
pub fn update_alloc(_alloc: *mut crate::mm::phys_alloc::BitmapAllocator) {}
