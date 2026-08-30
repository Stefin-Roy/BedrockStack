use super::PciDevice;
use super::caps::{self, PciCapability};
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
/// safe default since PCI init runs on the BSP. Single-vector (MME=0) path.
pub fn enable(dev: &PciDevice, cap: &PciCapability, vector: u8, dest_apic_id: u8) {
    enable_multi(dev, cap, vector, 1, dest_apic_id);
}

/// Program MSI for `count` contiguous vectors starting at `base_vector`.
///
/// `count` must be power-of-two 1..32 and aligned (`base_vector % count == 0`);
/// if not power-of-two it is rounded down to the nearest power-of-two that
/// fits the device's MMC. MME is set accordingly (log2 count). Returns
/// `true` if programmed, `false` if `count` exceeds device MMC or alignment
/// fails.
pub fn enable_multi(
    dev: &PciDevice,
    cap: &PciCapability,
    base_vector: u8,
    count: usize,
    dest_apic_id: u8,
) -> bool {
    if count == 0 {
        return false;
    }
    let mc = caps::read_u16(dev, cap, MC_OFF);

    // Read MMC (number of messages the device can send) before we modify MC.
    let mmc_exp = ((mc >> 1) & 0x7) as usize;
    let mmc = 1usize << mmc_exp;
    // Clamp count to power-of-two <= MMC and <= 32.
    let mut req = count.next_power_of_two().min(mmc).min(32);
    // Shrink until power-of-two and fits alignment and vector range.
    while req > 0 && (req & (req - 1) != 0) {
        req >>= 1;
    }
    if count != req && count != 0 {
        SerialPort::puts("[msi] WARN: count ");
        SerialPort::put_u64(count as u64);
        SerialPort::puts(" clamped to ");
        SerialPort::put_u64(req as u64);
        SerialPort::puts(" (mmc=");
        SerialPort::put_u64(mmc as u64);
        SerialPort::puts(")\n");
    }
    if req == 0 {
        return false;
    }
    if (base_vector as usize) % req != 0 {
        SerialPort::puts("[msi] FAIL: base_vector not aligned for count\n");
        return false;
    }
    if (base_vector as usize) + req > 256 {
        SerialPort::puts("[msi] FAIL: vector range overflow\n");
        return false;
    }
    let mme: u16 = (req as u16).trailing_zeros() as u16;

    SerialPort::puts("[msi] enabling: base_vector=");
    SerialPort::put_u64(base_vector as u64);
    SerialPort::puts(" count=");
    SerialPort::put_u64(req as u64);
    SerialPort::puts(" mme=");
    SerialPort::put_u64(mme as u64);
    SerialPort::puts(" dest_apic_id=");
    SerialPort::put_u64(dest_apic_id as u64);
    SerialPort::puts(" mmc=");
    SerialPort::put_u64(mmc as u64);
    SerialPort::puts("\n");

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
        let data: u16 = base_vector as u16;
        caps::write_u16(dev, cap, MD_OFF_64, data);
    } else {
        // 32-bit: write Message Data at +8.
        let data: u16 = base_vector as u16;
        caps::write_u16(dev, cap, MD_OFF_32, data);
    }

    // Step 3: Enable memory space, Bus Master, and disable INTx in PCI
    // Command register.  Bus Master is required for MSI memory writes.
    let pci_cfg = cfg();
    let cmd = pci_cfg.read16(dev.segment, dev.bus, dev.device, dev.function, 0x04);
    pci_cfg.write16(
        dev.segment,
        dev.bus,
        dev.device,
        dev.function,
        0x04,
        cmd | (1 << 1) | (1 << 2) | (1 << 10),
    );

    // Step 4: Set MME + enable bit.
    let mc_on = mc_disabled | 1 | (mme << 4);
    caps::write_u16(dev, cap, MC_OFF, mc_on);

    // Step 5: If the device supports Per-Vector Masking, unmask requested vectors.
    if mc & (1 << 8) != 0 {
        let mask_off = if mc & (1 << 7) != 0 { 16u16 } else { 12u16 };
        // Mask = 0 means all unmasked; for multi-vector we still need low `req` bits clear.
        // Keep full 0 for now (unmasks up to 32 vectors) – narrower mask would be
        //  (!((1<<req)-1)) but 0 is always safe and maximally permissive.
        caps::write_u32(dev, cap, mask_off, 0);
    }

    SerialPort::puts("[msi] enabled\n");
    true
}

/// Disable MSI for a device.
pub fn disable(dev: &PciDevice, cap: &PciCapability) {
    let mc = caps::read_u16(dev, cap, MC_OFF);
    caps::write_u16(dev, cap, MC_OFF, mc & !1);
}
