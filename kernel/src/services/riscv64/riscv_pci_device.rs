use super::super::pci_config::PciConfigSpace;
use super::super::pci_device::{Bar, PciCapability, PciDevice, PciDeviceManager};

pub struct RiscvPciDevice;



impl PciDeviceManager for RiscvPciDevice {
    fn devices(&self) -> &[PciDevice] {
        &[]
    }

    fn bar(&self, _dev: &PciDevice, _index: usize) -> Bar {
        Bar::Unused
    }

    fn find_capability(&self, _dev: &PciDevice, _cap_id: u8) -> Option<PciCapability> {
        None
    }

    fn cfg(&self) -> &dyn PciConfigSpace {
        &super::super::ecam_pci_config::EcamPciConfig
    }

    fn configure_msi(&self, _dev: &PciDevice, _cap: &PciCapability, _vector: u8, _dest_apic_id: u8) {}

    fn disable_msi(&self, _dev: &PciDevice, _cap: &PciCapability) {}

    fn configure_msix(
        &self,
        _dev: &PciDevice,
        _cap: &PciCapability,
        _bar_va: u64,
        _pba_va: u64,
        _table_entries: u16,
        _vector: u8,
        _dest_apic_id: u8,
    ) {}

    fn program_msix_entry(
        &self,
        _dev: &PciDevice,
        _cap: &PciCapability,
        _bar_va: u64,
        _entry_index: u16,
        _vector: u8,
        _dest_apic_id: u8,
    ) {}

    fn disable_msix(&self, _dev: &PciDevice, _cap: &PciCapability) {}

    fn msix_table_info(&self, _dev: &PciDevice, _cap: &PciCapability) -> crate::pci::msix::MsixInfo {
        crate::pci::msix::MsixInfo { table_size: 0, bir: 0, table_offset: 0, pba_bir: 0, pba_offset: 0 }
    }

    fn read_config_u8(&self, _dev: &PciDevice, _off: u16) -> u8 { 0 }
    fn read_config_u16(&self, _dev: &PciDevice, _off: u16) -> u16 { 0 }
    fn read_config_u32(&self, _dev: &PciDevice, _off: u16) -> u32 { 0 }
    fn write_config_u8(&self, _dev: &PciDevice, _off: u16, _val: u8) {}
    fn write_config_u16(&self, _dev: &PciDevice, _off: u16, _val: u16) {}
    fn write_config_u32(&self, _dev: &PciDevice, _off: u16, _val: u32) {}
}

static RISCV_PCI_DEVICE: RiscvPciDevice = RiscvPciDevice;

pub fn init() -> &'static dyn PciDeviceManager {
    &RISCV_PCI_DEVICE
}
