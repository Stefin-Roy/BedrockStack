use alloc::vec::Vec;
use crate::acpi::platform::{AcpiError, PciConfigRegions, PciMcfgRegion};

fn r64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off], buf[off + 1], buf[off + 2], buf[off + 3],
        buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7],
    ])
}

fn r16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

/// Parse MCFG table into PCI config regions.
pub fn parse_mcfg(vaddr: u64, length: u32) -> Result<PciConfigRegions, AcpiError> {
    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, length as usize) };

    if raw[0..4] != [b'M', b'C', b'F', b'G'] {
        return Err(AcpiError::BadSignature);
    }

    // MCFG: SDT header (36) + reserved (8) = offset 44 where entries start
    // Each entry: 16 bytes
    let entry_start = 44;
    let entry_size: usize = 16;
    let count = (length as usize - entry_start) / entry_size;

    let mut regions = Vec::with_capacity(count);
    for i in 0..count {
        let off = entry_start + i * entry_size;
        regions.push(PciMcfgRegion {
            base_address: r64(raw, off),
            pci_segment_group: r16(raw, off + 8),
            bus_number_start: raw[off + 10],
            bus_number_end: raw[off + 11],
        });
    }

    Ok(PciConfigRegions { regions })
}
