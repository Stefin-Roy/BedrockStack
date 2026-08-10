use alloc::vec::Vec;

use crate::mm::vmm::PageFlags;

use super::super::acpi::{AcpiProvider, InterruptModel, PciConfigRegions, PlatformInfo};

pub struct RiscvAcpi;



impl AcpiProvider for RiscvAcpi {
    fn interrupt_model(&self) -> &InterruptModel {
        &InterruptModel::Unknown
    }

    fn pci_config_regions(&self) -> &PciConfigRegions {
        static EMPTY: spin::Once<PciConfigRegions> = spin::Once::new();
        EMPTY.call_once(|| PciConfigRegions { regions: Vec::new() })
    }

    fn platform_info(&self) -> Option<&PlatformInfo> {
        None
    }

    fn cpus(&self) -> &[(u32, bool)] {
        &[]
    }

    fn map_device_mmio(&self, _paddr: u64, _size: u64, _flags: PageFlags) -> u64 {
        0
    }
}

static RISCV_ACPI: RiscvAcpi = RiscvAcpi;

pub fn init() -> &'static dyn AcpiProvider {
    &RISCV_ACPI
}
