use crate::acpi::{AcpiSubsystem, InterruptModel, PciConfigRegions, PlatformInfo};
use crate::mm::vmm::PageFlags;

use super::super::acpi::AcpiProvider;
use super::super::capability::Capability;

pub struct X86Acpi {
    acpi: &'static AcpiSubsystem,
}

impl X86Acpi {
    pub fn new(acpi: &'static AcpiSubsystem) -> Self {
        X86Acpi { acpi }
    }
}

impl Capability for X86Acpi {
    fn name(&self) -> &str {
        "x86-acpi"
    }
}

impl AcpiProvider for X86Acpi {
    fn interrupt_model(&self) -> &InterruptModel {
        &self.acpi.interrupt_model
    }

    fn pci_config_regions(&self) -> &PciConfigRegions {
        &self.acpi.pci_config_regions
    }

    fn platform_info(&self) -> Option<&PlatformInfo> {
        Some(&self.acpi.platform_info)
    }

    fn cpus(&self) -> &[(u32, bool)] {
        &self.acpi.cpus
    }

    fn map_device_mmio(&self, paddr: u64, size: u64, flags: PageFlags) -> u64 {
        crate::acpi::map_device_mmio(paddr, size, flags)
    }
}
