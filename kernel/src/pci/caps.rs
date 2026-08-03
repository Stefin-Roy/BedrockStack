use super::PciDevice;

pub const CAP_MSI: u8 = 0x05;
pub const CAP_MSIX: u8 = 0x11;
pub const CAP_PCI_EXPRESS: u8 = 0x10;
pub const CAP_PM: u8 = 0x01;

/// A discovered PCI capability in config space.
#[derive(Debug, Clone, Copy)]
pub struct PciCapability {
    pub id: u8,
    /// Capability offset in config space (points to the ID byte).
    pub offset: u8,
}

/// Walk the capabilities list for a given device.
/// Returns all capabilities found, in list order.
fn cfg() -> crate::obj::clients::PciCfgClient {
    crate::obj::clients::PciCfgClient::driver_pci()
}

pub fn all(dev: &PciDevice) -> alloc::vec::Vec<PciCapability> {
    let pci_cfg = cfg();
    let mut caps = alloc::vec::Vec::new();
    let mut offset = dev.caps_ptr;
    while offset != 0 {
        let id = pci_cfg.read8(dev.segment, dev.bus, dev.device, dev.function, offset as u16);
        let next = pci_cfg.read8(dev.segment, dev.bus, dev.device, dev.function, (offset + 1) as u16);
        caps.push(PciCapability { id, offset });
        offset = next;
    }
    caps
}

/// Find the first capability matching `cap_id`, or `None`.
pub fn find(dev: &PciDevice, cap_id: u8) -> Option<PciCapability> {
    let pci_cfg = cfg();
    let mut offset = dev.caps_ptr;
    while offset != 0 {
        let id = pci_cfg.read8(dev.segment, dev.bus, dev.device, dev.function, offset as u16);
        if id == cap_id {
            return Some(PciCapability { id, offset });
        }
        offset = pci_cfg.read8(dev.segment, dev.bus, dev.device, dev.function, (offset + 1) as u16);
    }
    None
}

/// Check whether a device has a given capability.
pub fn has(dev: &PciDevice, cap_id: u8) -> bool {
    find(dev, cap_id).is_some()
}

/// Read a byte from a capability's data area.
pub fn read_u8(dev: &PciDevice, cap: &PciCapability, reg: u16) -> u8 {
    let pci_cfg = cfg();
    pci_cfg.read8(dev.segment, dev.bus, dev.device, dev.function, cap.offset as u16 + reg)
}

/// Read a u16 from a capability's data area.
pub fn read_u16(dev: &PciDevice, cap: &PciCapability, reg: u16) -> u16 {
    let pci_cfg = cfg();
    pci_cfg.read16(dev.segment, dev.bus, dev.device, dev.function, cap.offset as u16 + reg)
}

/// Read a u32 from a capability's data area.
pub fn read_u32(dev: &PciDevice, cap: &PciCapability, reg: u16) -> u32 {
    let pci_cfg = cfg();
    pci_cfg.read32(dev.segment, dev.bus, dev.device, dev.function, cap.offset as u16 + reg)
}

/// Write a byte to a capability's data area.
pub fn write_u8(dev: &PciDevice, cap: &PciCapability, reg: u16, val: u8) {
    let pci_cfg = cfg();
    pci_cfg.write8(dev.segment, dev.bus, dev.device, dev.function, cap.offset as u16 + reg, val);
}

/// Write a u16 to a capability's data area.
pub fn write_u16(dev: &PciDevice, cap: &PciCapability, reg: u16, val: u16) {
    let pci_cfg = cfg();
    pci_cfg.write16(dev.segment, dev.bus, dev.device, dev.function, cap.offset as u16 + reg, val);
}

/// Write a u32 to a capability's data area.
pub fn write_u32(dev: &PciDevice, cap: &PciCapability, reg: u16, val: u32) {
    let pci_cfg = cfg();
    pci_cfg.write32(dev.segment, dev.bus, dev.device, dev.function, cap.offset as u16 + reg, val);
}
