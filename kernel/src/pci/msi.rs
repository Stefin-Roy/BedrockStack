use super::caps::{self, PciCapability};
use super::PciDevice;
use crate::drivers::serial::SerialPort;

fn cfg() -> &'static dyn crate::services::pci_config::PciConfigSpace {
    crate::services::kernel_services().pci_cfg
}

/// Message Control register offset (from capability base).
const MC_OFF: u16 = 2;
/// Message Address register offset.
const MA_OFF: u16 = 4;
/// Message Upper Address offset (64-bit only).
const MUA_OFF: u16 = 8;
/// Message Data offset (32-bit: +8, 64-bit: +12).
const MD_OFF_32: u16 = 8;
const MD_OFF_64: u16 = 12;

/// Detect whether a device supports MSI and return the capability.
pub fn find_msi(dev: &PciDevice) -> Option<PciCapability> {
    caps::find(dev, caps::CAP_MSI)
}

/// Returns `(is_64bit, has_per_vector_masking)` for the MSI capability.
pub fn cap_info(dev: &PciDevice, cap: &PciCapability) -> (bool, bool) {
    let mc = caps::read_u16(dev, cap, MC_OFF);
    let is_64 = mc & (1 << 7) != 0;
    let pvm = mc & (1 << 8) != 0;
    (is_64, pvm)
}

/// Program MSI to deliver interrupts to `vector` on the given `dest_apic_id`.
///
/// `dest_apic_id` is the 8-bit destination APIC ID. The BSP's ID is a
/// safe default since PCI init runs on the BSP.
pub fn enable(dev: &PciDevice, cap: &PciCapability, vector: u8, dest_apic_id: u8) {
    let mc = caps::read_u16(dev, cap, MC_OFF);

    // Read MMC (number of messages the device can send) before we modify MC.
    let mmc = (mc >> 1) & 0x7;
    SerialPort::puts("[msi] enabling: vector=");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts(" dest_apic_id=");
    SerialPort::put_u64(dest_apic_id as u64);
    SerialPort::puts(" mmc=");
    SerialPort::put_u64(mmc as u64);
    SerialPort::puts("\n");

    // Number of requested vectors (MME = 000 → 1 vector).
    let mme: u16 = 0;

    // Step 1: Disable MSI and clear MME bits before touching address/data
    // (PCI spec requires MSI to be disabled while modifying these fields).
    let mc_disabled = mc & !(1 | (0x7 << 4));
    caps::write_u16(dev, cap, MC_OFF, mc_disabled);

    // Step 2: Program Message Address and Data.
    let addr: u32 = 0xFEE00000 | ((dest_apic_id as u32) << 12);
    caps::write_u32(dev, cap, MA_OFF, addr);

    if mc & (1 << 7) != 0 {
        // 64-bit: clear upper address.
        caps::write_u32(dev, cap, MUA_OFF, 0);
        // Write Message Data: vector in lower 8 bits, delivery mode = fixed (000).
        let data: u16 = vector as u16;
        caps::write_u16(dev, cap, MD_OFF_64, data);
    } else {
        // 32-bit: write Message Data at +8.
        let data: u16 = vector as u16;
        caps::write_u16(dev, cap, MD_OFF_32, data);
    }

    // Step 3: Enable memory space, Bus Master, and disable INTx in PCI
    // Command register.  Bus Master is required for MSI memory writes.
    let pci_cfg = cfg();
    let cmd = pci_cfg.read16(dev.segment, dev.bus, dev.device, dev.function, 0x04);
    pci_cfg.write16(
        dev.segment, dev.bus, dev.device, dev.function, 0x04,
        cmd | (1 << 1) | (1 << 2) | (1 << 10),
    );

    // Step 4: Set MME + enable bit.
    let mc_on = mc_disabled | 1 | (mme << 4);
    caps::write_u16(dev, cap, MC_OFF, mc_on);

    // Step 5: If the device supports Per-Vector Masking, unmask vector 0.
    if mc & (1 << 8) != 0 {
        let mask_off = if mc & (1 << 7) != 0 { 16u16 } else { 12u16 };
        caps::write_u32(dev, cap, mask_off, 0);
    }

    SerialPort::puts("[msi] enabled\n");
}

/// Disable MSI for a device.
pub fn disable(dev: &PciDevice, cap: &PciCapability) {
    let mc = caps::read_u16(dev, cap, MC_OFF);
    caps::write_u16(dev, cap, MC_OFF, mc & !1);
}
