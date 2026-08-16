pub mod bar;
pub mod caps;
pub mod ecam;
pub mod enumerate;
pub mod msi;
pub mod msix;

use crate::acpi::PciConfigRegions;
use crate::mm::phys_alloc::BitmapAllocator;
use alloc::vec::Vec;

/// A discovered PCI(e) device / function.
#[derive(Debug, Clone, Copy)]
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
    /// Bitmask of BAR slots consumed by a preceding 64-bit BAR (bit i set =
    /// slot i is the upper half of a 64-bit BAR at i-1).
    pub bars_consumed: u8,
    pub caps_ptr: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
}

/// Initialise the PCI subsystem.
///
/// 1. Store VMM state for mapping ECAM regions.
/// 2. Map all MCFG ECAM regions into the virtual address space.
/// 3. Enumerate all buses on every segment group and discover devices.
///
/// Must be called once after the page tables are live and ACPI is initialised.
pub fn init(regions: &PciConfigRegions, root: u64, alloc: *mut BitmapAllocator) {
    ecam::init_vmm(root, alloc);
    ecam::map_all(regions);

    // Enumerate every segment group present in the MCFG.  `regions` may
    // contain multiple entries per segment (one per bus range); dedupe to
    // unique segment groups before scanning.
    let mut segments: Vec<u16> = Vec::new();
    for r in &regions.regions {
        if !segments.contains(&r.pci_segment_group) {
            segments.push(r.pci_segment_group);
        }
    }
    enumerate::enumerate_all(&segments);

    let count = enumerate::all().len();
    crate::drivers::serial::SerialPort::puts("[pci] init complete: ");
    crate::drivers::serial::SerialPort::put_u64(count as u64);
    crate::drivers::serial::SerialPort::puts(" devices found\n");
}

/// Return a reference to the list of all discovered PCI devices.
pub fn devices() -> &'static [PciDevice] {
    enumerate::all()
}

/// Enable the device's memory space and bus master in the PCI Command
/// register (offset 0x04, bits 1 and 2).
///
/// Firmware (OVMF, most BIOSes) configures boot-critical devices, but a
/// real machine frequently hands the HDA/audio function over with memory
/// decode still disabled — BAR reads then return 0xFF (all ones) until this
/// is set.  QEMU hides the problem because OVMF enables every device during
/// POST.  Mirrors Linux `pci_enable_device()`.
pub fn enable_device(dev: &PciDevice) {
    let pci_cfg = crate::services::kernel_services().pci_cfg;
    let cmd = pci_cfg.read16(dev.segment, dev.bus, dev.device, dev.function, 0x04);
    pci_cfg.write16(
        dev.segment,
        dev.bus,
        dev.device,
        dev.function,
        0x04,
        cmd | (1 << 1) | (1 << 2),
    );
}
