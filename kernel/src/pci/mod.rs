pub mod ecam;
pub mod enumerate;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::acpi::PciConfigRegions;

/// A discovered PCI(e) device / function.
#[derive(Debug, Clone)]
pub struct PciDevice {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub bars: [u32; 6],
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
}

/// Initialise the PCI subsystem.
///
/// 1. Store VMM state for mapping ECAM regions.
/// 2. Map all MCFG ECAM regions into the virtual address space.
/// 3. Enumerate all buses and discover devices.
///
/// Must be called once after the page tables are live and ACPI is initialised.
pub fn init(regions: &PciConfigRegions, root: u64, alloc: *mut BitmapAllocator) {
    ecam::init_vmm(root, alloc);
    ecam::map_all(regions);

    // Scan segment group 0 (the common case; multi-segment support can be
    // added by iterating `regions.regions` for unique segment groups).
    enumerate::enumerate(0);

    let count = enumerate::all().len();
    crate::drivers::serial::SerialPort::puts("[pci] init complete: ");
    crate::drivers::serial::SerialPort::put_u64(count as u64);
    crate::drivers::serial::SerialPort::puts(" devices found\n");
}

/// Return a reference to the list of all discovered PCI devices.
pub fn devices() -> &'static [PciDevice] {
    enumerate::all()
}
