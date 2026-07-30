use crate::mm::vmm::PageFlags;

use super::capability::Capability;

pub use crate::acpi::{InterruptModel, IoApic, PciConfigRegions, PlatformInfo};

pub trait AcpiProvider: Capability {
    fn interrupt_model(&self) -> &InterruptModel;
    fn pci_config_regions(&self) -> &PciConfigRegions;
    fn platform_info(&self) -> Option<&PlatformInfo>;
    fn cpus(&self) -> &[(u32, bool)];

    /// Map a physical MMIO region and return its virtual address.
    fn map_device_mmio(&self, paddr: u64, size: u64, flags: PageFlags) -> u64;
}
