use alloc::vec::Vec;
use spin::Mutex;

use crate::pci::ecam;
use crate::pci::PciDevice;

static DEVICES: Mutex<Option<Vec<PciDevice>>> = Mutex::new(None);

pub fn all() -> &'static [PciDevice] {
    let guard = DEVICES.lock();
    let vec = guard.as_ref().expect("PCI not enumerated yet");
    unsafe { core::mem::transmute::<&[PciDevice], &[PciDevice]>(vec.as_slice()) }
}

pub fn enumerate(segment: u16) {
    let mut devices = Vec::new();
    scan_bus(segment, 0, &mut devices);
    *DEVICES.lock() = Some(devices);
}

fn scan_bus(segment: u16, bus: u8, devices: &mut Vec<PciDevice>) {
    for device in 0..32 {
        let vendor = ecam::read_u16(segment, bus, device, 0, 0x00);
        if vendor == 0xFFFF {
            continue;
        }

        let header_type = ecam::read_u8(segment, bus, device, 0, 0x0E);

        // Function 0
        read_function(segment, bus, device, 0, devices);

        if header_type & 0x80 != 0 {
            for function in 1..8 {
                let v = ecam::read_u16(segment, bus, device, function, 0x00);
                if v != 0xFFFF {
                    read_function(segment, bus, device, function, devices);
                }
            }
        }
    }
}

fn read_function(segment: u16, bus: u8, device: u8, function: u8, devices: &mut Vec<PciDevice>) {
    let vendor_id = ecam::read_u16(segment, bus, device, function, 0x00);
    let device_id = ecam::read_u16(segment, bus, device, function, 0x02);
    let revision = ecam::read_u8(segment, bus, device, function, 0x08);
    let prog_if = ecam::read_u8(segment, bus, device, function, 0x09);
    let subclass = ecam::read_u8(segment, bus, device, function, 0x0A);
    let class = ecam::read_u8(segment, bus, device, function, 0x0B);

    let mut bars = [0u32; 6];
    for i in 0..6 {
        bars[i] = ecam::read_u32(segment, bus, device, function, 0x10 + (i as u16) * 4);
    }

    let caps_ptr = ecam::read_u8(segment, bus, device, function, 0x34);
    let interrupt_line = ecam::read_u8(segment, bus, device, function, 0x3C);
    let interrupt_pin = ecam::read_u8(segment, bus, device, function, 0x3D);

    let pci_dev = PciDevice {
        segment,
        bus,
        device,
        function,
        vendor_id,
        device_id,
        revision,
        class,
        subclass,
        prog_if,
        bars,
        caps_ptr,
        interrupt_line,
        interrupt_pin,
    };

    // If this is a PCI-PCI bridge, recursively scan the secondary bus
    if class == 0x06 && subclass == 0x04 {
        let secondary_bus = ecam::read_u8(segment, bus, device, function, 0x19);
        if secondary_bus != bus {
            scan_bus(segment, secondary_bus, devices);
        }
    }

    devices.push(pci_dev);
}
