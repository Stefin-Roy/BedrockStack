use super::super::pci_config::PciConfigSpace;
use super::super::pci_device::{Bar, PciCapability, PciDevice, PciDeviceManager};

pub struct X86PciDevice;

impl PciDeviceManager for X86PciDevice {
    fn devices(&self) -> &[PciDevice] {
        crate::pci::devices()
    }

    fn bar(&self, dev: &PciDevice, index: usize) -> Bar {
        crate::pci::bar::bar(dev, index)
    }

    fn find_capability(&self, dev: &PciDevice, cap_id: u8) -> Option<PciCapability> {
        crate::pci::caps::find(dev, cap_id)
    }

    fn cfg(&self) -> &dyn PciConfigSpace {
        &super::super::ecam_pci_config::EcamPciConfig
    }

    fn configure_msi(&self, dev: &PciDevice, cap: &PciCapability, vector: u8, dest_apic_id: u8) {
        crate::pci::msi::enable(dev, cap, vector, dest_apic_id);
    }

    fn disable_msi(&self, dev: &PciDevice, cap: &PciCapability) {
        crate::pci::msi::disable(dev, cap);
    }

    fn configure_msix(
        &self,
        dev: &PciDevice,
        cap: &PciCapability,
        bar_va: u64,
        pba_va: u64,
        table_entries: u16,
        vector: u8,
        dest_apic_id: u8,
    ) {
        crate::pci::msix::enable(dev, cap, bar_va, pba_va, table_entries, vector, dest_apic_id);
    }

    fn program_msix_entry(
        &self,
        dev: &PciDevice,
        cap: &PciCapability,
        bar_va: u64,
        entry_index: u16,
        vector: u8,
        dest_apic_id: u8,
    ) {
        crate::pci::msix::program_entry(dev, cap, bar_va, entry_index, vector, dest_apic_id);
    }

    fn disable_msix(&self, dev: &PciDevice, cap: &PciCapability) {
        crate::pci::msix::disable(dev, cap);
    }

    fn msix_table_info(&self, dev: &PciDevice, cap: &PciCapability) -> crate::pci::msix::MsixInfo {
        crate::pci::msix::table_info(dev, cap)
    }

    fn read_config_u8(&self, dev: &PciDevice, off: u16) -> u8 {
        crate::pci::ecam::read_u8(dev.segment, dev.bus, dev.device, dev.function, off)
    }

    fn read_config_u16(&self, dev: &PciDevice, off: u16) -> u16 {
        crate::pci::ecam::read_u16(dev.segment, dev.bus, dev.device, dev.function, off)
    }

    fn read_config_u32(&self, dev: &PciDevice, off: u16) -> u32 {
        crate::pci::ecam::read_u32(dev.segment, dev.bus, dev.device, dev.function, off)
    }

    fn write_config_u8(&self, dev: &PciDevice, off: u16, val: u8) {
        crate::pci::ecam::write_u8(dev.segment, dev.bus, dev.device, dev.function, off, val);
    }

    fn write_config_u16(&self, dev: &PciDevice, off: u16, val: u16) {
        crate::pci::ecam::write_u16(dev.segment, dev.bus, dev.device, dev.function, off, val);
    }

    fn write_config_u32(&self, dev: &PciDevice, off: u16, val: u32) {
        crate::pci::ecam::write_u32(dev.segment, dev.bus, dev.device, dev.function, off, val);
    }
}

static X86_PCI_DEVICE: X86PciDevice = X86PciDevice;

pub fn init() -> &'static dyn PciDeviceManager {
    &X86_PCI_DEVICE
}
