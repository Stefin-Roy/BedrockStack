use alloc::boxed::Box;
use alloc::vec::Vec;

use spin::Once;

use crate::pci::PciDevice;

fn cfg() -> crate::obj::clients::PciCfgClient {
    crate::obj::clients::PciCfgClient::driver_pci()
}

/// The discovered PCI device census — the interior of [`crate::pci::devices`]
/// and the `pci:forest` `PciForestNode` (obj/devices.rs §7.11.4). Set once by
/// `enumerate_all()` and never freed. Not a public enumeration API:
/// capability-gated consumers must go through the forest node's `count`/
/// `children` hooks, not this slice.
static DEVICES: Once<&'static [PciDevice]> = Once::new();

/// Interior accessor for the `pci:forest` census: the full discovered device
/// list. See `DEVICES`. Crate-internal so the ambient slice stays out of the
/// library's public surface; the forest node and the driver sweep read it
/// through `crate::pci::devices`.
pub(crate) fn all() -> &'static [PciDevice] {
    *DEVICES.get().expect("PCI not enumerated yet")
}

/// Enumerate every segment group in `segments`, merging the results into a
/// single leaked device list.
pub fn enumerate_all(segments: &[u16]) {
    let mut devices = Vec::new();
    for &segment in segments {
        scan_bus(segment, 0, &mut devices);
    }
    let leaked: &'static [PciDevice] = Box::leak(devices.into_boxed_slice());
    DEVICES.call_once(|| leaked);
}

fn scan_bus(segment: u16, bus: u8, devices: &mut Vec<PciDevice>) {
    let pci_cfg = cfg();
    for device in 0..32 {
        let vendor = pci_cfg.read16(segment, bus, device, 0, 0x00);
        if vendor == 0xFFFF {
            continue;
        }

        let header_type = pci_cfg.read8(segment, bus, device, 0, 0x0E);

        // Function 0
        read_function(segment, bus, device, 0, devices);

        if header_type & 0x80 != 0 {
            for function in 1..8 {
                let v = pci_cfg.read16(segment, bus, device, function, 0x00);
                if v != 0xFFFF {
                    read_function(segment, bus, device, function, devices);
                }
            }
        }
    }
}

fn read_function(segment: u16, bus: u8, device: u8, function: u8, devices: &mut Vec<PciDevice>) {
    let pci_cfg = cfg();
    let vendor_id = pci_cfg.read16(segment, bus, device, function, 0x00);
    let device_id = pci_cfg.read16(segment, bus, device, function, 0x02);
    let revision = pci_cfg.read8(segment, bus, device, function, 0x08);
    let prog_if = pci_cfg.read8(segment, bus, device, function, 0x09);
    let subclass = pci_cfg.read8(segment, bus, device, function, 0x0A);
    let class = pci_cfg.read8(segment, bus, device, function, 0x0B);

    let mut bars = [0u32; 6];
    let mut bars_consumed: u8 = 0;
    for i in 0..6 {
        bars[i] = pci_cfg.read32(segment, bus, device, function, 0x10 + (i as u16) * 4);
        if i < 5 && bars[i] & 1 == 0 && (bars[i] & 0x06) == 4 {
            bars_consumed |= 1 << (i + 1);
        }
    }

    let caps_ptr = pci_cfg.read8(segment, bus, device, function, 0x34);
    let interrupt_line = pci_cfg.read8(segment, bus, device, function, 0x3C);
    let interrupt_pin = pci_cfg.read8(segment, bus, device, function, 0x3D);

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
        bars_consumed,
        caps_ptr,
        interrupt_line,
        interrupt_pin,
    };

    // If this is a PCI-PCI bridge, recursively scan the secondary bus
    if class == 0x06 && subclass == 0x04 {
        let secondary_bus = pci_cfg.read8(segment, bus, device, function, 0x19);
        if secondary_bus != bus {
            scan_bus(segment, secondary_bus, devices);
        }
    }

    devices.push(pci_dev);
}
