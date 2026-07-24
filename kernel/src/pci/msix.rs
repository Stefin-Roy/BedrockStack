use core::ptr::{read_volatile, write_volatile};

use super::caps::{self, PciCapability};
use super::PciDevice;
use crate::drivers::serial::SerialPort;

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
    // Table Size: N-1 encoded in bits 10:7.
    let table_size = ((mc >> 7) & 0xF) + 1;

    let tbl = caps::read_u32(dev, cap, 4);
    let bir = (tbl & 0x7) as usize;
    let table_offset = (tbl & 0xFFFF_FFF8) as u64;

    let pba = caps::read_u32(dev, cap, 8);
    let pba_bir = (pba & 0x7) as usize;
    let pba_offset = (pba & 0xFFFF_FFF8) as u64;

    MsixInfo { table_size, bir, table_offset, pba_bir, pba_offset }
}

/// Program MSI-X table entries at `table_va` (the virtual address of the
/// MSI-X table within the mapped BAR). Each entry gets the same vector
/// (single-vector mode).
///
/// The caller must have already mapped the BAR containing the MSI-X table
/// into the virtual address space (using `DmaAllocator::map_mmio()` or
/// equivalent).
///
/// `table_entries` is the number of entries to program (<= `info.table_size`).
pub fn enable(
    dev: &PciDevice,
    cap: &PciCapability,
    info: &MsixInfo,
    table_va: u64,
    table_entries: u16,
    vector: u8,
    dest_apic_id: u8,
) {
    let mc = caps::read_u16(dev, cap, 2);

    // Disable + function mask while programming.
    caps::write_u16(dev, cap, 2, mc | 3);

    let addr: u64 = 0xFEE00000 | ((dest_apic_id as u64) << 12);
    let data: u32 = vector as u32;

    let table = table_va as *mut MsixTableEntry;
    let count = table_entries.min(info.table_size);
    for i in 0..count {
        unsafe {
            write_volatile(&mut (*table.add(i as usize)).msg_addr, addr);
            write_volatile(&mut (*table.add(i as usize)).msg_data, data);
            // Clear mask bit to enable this entry.
            write_volatile(&mut (*table.add(i as usize)).vector_ctrl, 0);
        }
    }

    SerialPort::puts("[msix] enabled: vector=");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts(" entries=");
    SerialPort::put_u64(count as u64);
    SerialPort::puts("\n");

    // Enable MSI-X, clear function mask.
    let mc_on = (mc | 1) & !2;
    caps::write_u16(dev, cap, 2, mc_on);
}

/// Disable MSI-X for a device.
pub fn disable(dev: &PciDevice, cap: &PciCapability) {
    let mc = caps::read_u16(dev, cap, 2);
    caps::write_u16(dev, cap, 2, mc & !1);
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
