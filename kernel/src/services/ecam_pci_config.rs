use super::capability::Capability;
use super::pci_config::PciConfigSpace;

/// ECAM-based PCI config space access, shared across both architectures.
pub struct EcamPciConfig;

impl Capability for EcamPciConfig {
    fn name(&self) -> &str {
        "ecam-pci-config"
    }
}

impl PciConfigSpace for EcamPciConfig {
    fn read8(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u8 {
        crate::pci::ecam::read_u8(seg, bus, dev, func, off)
    }

    fn read16(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u16 {
        crate::pci::ecam::read_u16(seg, bus, dev, func, off)
    }

    fn read32(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u32 {
        crate::pci::ecam::read_u32(seg, bus, dev, func, off)
    }

    fn write8(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u8) {
        crate::pci::ecam::write_u8(seg, bus, dev, func, off, val);
    }

    fn write16(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u16) {
        crate::pci::ecam::write_u16(seg, bus, dev, func, off, val);
    }

    fn write32(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u32) {
        crate::pci::ecam::write_u32(seg, bus, dev, func, off, val);
    }
}

static ECAM_PCI_CONFIG: EcamPciConfig = EcamPciConfig;

pub fn init() -> &'static dyn PciConfigSpace {
    &ECAM_PCI_CONFIG as &'static dyn PciConfigSpace
}

/// C5: return the concrete ECAM node as a `'static` object for obj-endowment.
pub fn ecam_static() -> &'static EcamPciConfig {
    &ECAM_PCI_CONFIG
}
