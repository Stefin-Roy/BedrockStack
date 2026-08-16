use super::pci_config::PciConfigSpace;

pub use crate::pci::PciDevice;
pub use crate::pci::bar::Bar;
pub use crate::pci::caps::PciCapability;

pub trait PciDeviceManager: Send + Sync {
    fn devices(&self) -> &[PciDevice];
    fn bar(&self, dev: &PciDevice, index: usize) -> Bar;
    fn find_capability(&self, dev: &PciDevice, cap_id: u8) -> Option<PciCapability>;
    fn has_capability(&self, dev: &PciDevice, cap_id: u8) -> bool {
        self.find_capability(dev, cap_id).is_some()
    }
    fn cfg(&self) -> &dyn PciConfigSpace;
    fn configure_msi(&self, dev: &PciDevice, cap: &PciCapability, vector: u8, dest_apic_id: u8);
    fn configure_msix(
        &self,
        dev: &PciDevice,
        cap: &PciCapability,
        bar_va: u64,
        pba_va: u64,
        table_entries: u16,
        vector: u8,
        dest_apic_id: u8,
    );
    fn program_msix_entry(
        &self,
        dev: &PciDevice,
        cap: &PciCapability,
        bar_va: u64,
        entry_index: u16,
        vector: u8,
        dest_apic_id: u8,
    );
    fn disable_msi(&self, dev: &PciDevice, cap: &PciCapability);
    fn disable_msix(&self, dev: &PciDevice, cap: &PciCapability);
    fn msix_table_info(&self, dev: &PciDevice, cap: &PciCapability) -> crate::pci::msix::MsixInfo;
    fn read_config_u8(&self, dev: &PciDevice, off: u16) -> u8;
    fn read_config_u16(&self, dev: &PciDevice, off: u16) -> u16;
    fn read_config_u32(&self, dev: &PciDevice, off: u16) -> u32;
    fn write_config_u8(&self, dev: &PciDevice, off: u16, val: u8);
    fn write_config_u16(&self, dev: &PciDevice, off: u16, val: u16);
    fn write_config_u32(&self, dev: &PciDevice, off: u16, val: u32);
}
