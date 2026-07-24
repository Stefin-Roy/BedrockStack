use core::ptr::{read_volatile, write_volatile};

use super::bar::Bar;
use super::caps::{self, PciCapability};
use super::PciDevice;
use crate::drivers::serial::SerialPort;

/// MSI-X Message Control register bits (capability offset +2).
const MC_MSIX_ENABLE: u16 = 1 << 14;
const MC_FUNCTION_MASK: u16 = 1 << 15;

/// MSI-X table entry (16 bytes, in device BAR space).
#[repr(C)]
struct MsixTableEntry {
    msg_addr: u64,
    msg_data: u32,
    vector_ctrl: u32,
}

/// Information parsed from the MSI-X capability.
pub struct MsixInfo {
    pub table_size: u16,
    pub bir: usize,
    pub table_offset: u64,
    pub pba_bir: usize,
    pub pba_offset: u64,
}

/// Parse the MSI-X capability to extract table and PBA location.
pub fn table_info(dev: &PciDevice, cap: &PciCapability) -> MsixInfo {
    let mc = caps::read_u16(dev, cap, 2);
    let table_size = ((mc >> 7) & 0xF) + 1;

    let tbl = caps::read_u32(dev, cap, 4);
    let bir = (tbl & 0x7) as usize;
    let table_offset = (tbl & 0xFFFF_FFF8) as u64;

    let pba = caps::read_u32(dev, cap, 8);
    let pba_bir = (pba & 0x7) as usize;
    let pba_offset = (pba & 0xFFFF_FFF8) as u64;

    MsixInfo { table_size, bir, table_offset, pba_bir, pba_offset }
}

/// Enable MSI-X for a device.
///
/// `bar_va` is the virtual address of the mapped BAR that contains the
/// MSI-X table (the BAR index is read from the MSI-X capability). The
/// table offset within that BAR is computed internally.
///
/// The BAR is validated via `pci::bar::bar()` — it must be a memory BAR
/// or the function returns early without enabling MSI-X.
pub fn enable(
    dev: &PciDevice,
    cap: &PciCapability,
    bar_va: u64,
    table_entries: u16,
    vector: u8,
    dest_apic_id: u8,
) {
    let info = table_info(dev, cap);

    // Validate the table's BAR is memory-mapped.
    match super::bar::bar(dev, info.bir) {
        Bar::Memory { .. } => {}
        _ => {
            SerialPort::puts("[msix] table BAR is not memory-mapped, cannot enable\n");
            return;
        }
    }

    let mc = caps::read_u16(dev, cap, 2);

    // Disable MSI-X + set Function Mask while programming the table.
    caps::write_u16(dev, cap, 2, (mc & !MC_MSIX_ENABLE) | MC_FUNCTION_MASK);

    let addr: u64 = 0xFEE00000 | ((dest_apic_id as u64) << 12);
    let data: u32 = vector as u32;
    let count = table_entries.min(info.table_size);
    let table_va = bar_va + info.table_offset;

    let table = table_va as *mut MsixTableEntry;
    for i in 0..count {
        unsafe {
            write_volatile(&mut (*table.add(i as usize)).msg_addr, addr);
            write_volatile(&mut (*table.add(i as usize)).msg_data, data);
            write_volatile(&mut (*table.add(i as usize)).vector_ctrl, 0);
        }
    }

    SerialPort::puts("[msix] enabled: vector=");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts(" entries=");
    SerialPort::put_u64(count as u64);
    SerialPort::puts("\n");

    // Enable MSI-X, clear Function Mask.
    let mc_on = (mc & !MC_FUNCTION_MASK) | MC_MSIX_ENABLE;
    caps::write_u16(dev, cap, 2, mc_on);
}

/// Disable MSI-X for a device.
pub fn disable(dev: &PciDevice, cap: &PciCapability) {
    let mc = caps::read_u16(dev, cap, 2);
    caps::write_u16(dev, cap, 2, mc & !MC_MSIX_ENABLE);
}

/// Read the Pending Bit Array for a given entry index.
/// Returns true if there is a pending interrupt for entry `index`.
///
/// `pba_va` is the mapped virtual address of the PBA within the device's BAR.
pub fn pending(pba_va: u64, index: u16) -> bool {
    let word = (index / 64) as usize;
    let bit = index % 64;
    unsafe {
        let pba = pba_va as *const u64;
        read_volatile(pba.add(word)) & (1u64 << bit) != 0
    }
}
