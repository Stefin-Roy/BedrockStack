use super::caps::{self, PciCapability};
use super::PciDevice;
use crate::drivers::serial::SerialPort;

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

    // Number of requested vectors (MME = 000 → 1 vector).
    let mme: u16 = 0;
    // Clear enable bit first.
    let mc_off = mc & !1;

    // Read MMC (number of messages the device can send).
    let mmc = (mc >> 1) & 0x7;
    SerialPort::puts("[msi] enabling: vector=");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts(" dest_apic_id=");
    SerialPort::put_u64(dest_apic_id as u64);
    SerialPort::puts(" mmc=");
    SerialPort::put_u64(mmc as u64);
    SerialPort::puts("\n");

    // Write Message Address: 0xFEE00000 | (apic_id << 12)
    // Physical mode, no redirection hint.
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

    // Set MME + enable bit.
    let mc_on = mc_off | 1 | (mme << 4);
    caps::write_u16(dev, cap, MC_OFF, mc_on);

    SerialPort::puts("[msi] enabled\n");
}

/// Disable MSI for a device.
pub fn disable(dev: &PciDevice, cap: &PciCapability) {
    let mc = caps::read_u16(dev, cap, MC_OFF);
    caps::write_u16(dev, cap, MC_OFF, mc & !1);
}
