use super::PciDevice;

/// Decoded PCI BAR.
#[derive(Debug, Clone, Copy)]
pub enum Bar {
    /// Slot is not present (out of range, consumed by preceding 64-bit BAR,
    /// or reserved encoding).
    Unused,

    /// Memory-mapped I/O BAR.
    Memory { addr: u64, prefetchable: bool },

    /// Port I/O BAR.
    Io { port: u32 },
}

/// Probe the size of a memory BAR via the standard sizing trick
/// (write all-ones, read back mask). Returns `None` for I/O BARs or when
/// the BAR is unimplemented. Caller must have `pci_cfg` available.
pub fn bar_size(dev: &PciDevice, index: usize) -> Option<u64> {
    if index >= 6 || (dev.bars_consumed & (1 << index)) != 0 {
        return None;
    }
    let raw = dev.bars[index];
    if raw & 1 == 1 {
        return None; // I/O BAR — sizing different, not needed for xHCI
    }
    // Memory BAR: determine 32- vs 64-bit, then size via config space.
    let cfg = crate::services::kernel_services().pci_cfg;
    let off = 0x10 + (index as u16) * 4;
    let orig = cfg.read32(dev.segment, dev.bus, dev.device, dev.function, off);
    // Write all ones to get mask
    cfg.write32(dev.segment, dev.bus, dev.device, dev.function, off, 0xFFFF_FFFF);
    let mask = cfg.read32(dev.segment, dev.bus, dev.device, dev.function, off);
    // Restore original
    cfg.write32(dev.segment, dev.bus, dev.device, dev.function, off, orig);
    if mask == 0 || mask == 0xFFFF_FFFF {
        return None;
    }
    let is_64 = (orig & 0x06) == 4;
    if is_64 {
        if index == 5 {
            return None;
        }
        let orig_hi = cfg.read32(dev.segment, dev.bus, dev.device, dev.function, off + 4);
        cfg.write32(dev.segment, dev.bus, dev.device, dev.function, off + 4, 0xFFFF_FFFF);
        let mask_hi = cfg.read32(dev.segment, dev.bus, dev.device, dev.function, off + 4);
        cfg.write32(dev.segment, dev.bus, dev.device, dev.function, off + 4, orig_hi);
        let mask64 = (mask as u64) | ((mask_hi as u64) << 32);
        let size_mask = mask64 & 0xFFFF_FFFF_FFFF_FFF0;
        if size_mask == 0 {
            return None;
        }
        Some((!size_mask).wrapping_add(1))
    } else {
        let size_mask = mask & 0xFFFF_FFF0;
        if size_mask == 0 {
            return None;
        }
        Some((!size_mask as u64).wrapping_add(1) & 0xFFFF_FFFF)
    }
}

/// Decode a PCI BAR slot into its semantic type and address.
///
/// `index` is 0-based (BAR0–BAR5, corresponding to config offsets 0x10–0x24).
///
/// A 64-bit memory BAR consumes two consecutive slots. Slot `i+1` is marked
/// as consumed via `bars_consumed` (set during enumeration) and returns
/// `Unused` regardless of its raw value.
pub fn bar(dev: &PciDevice, index: usize) -> Bar {
    if index >= 6 || (dev.bars_consumed & (1 << index)) != 0 {
        return Bar::Unused;
    }

    let raw = dev.bars[index];
    if raw & 1 == 1 {
        return Bar::Io { port: raw & !3 };
    }

    let p = raw & 8 != 0;
    match raw & 0x06 {
        0 => Bar::Memory {
            addr: (raw & 0xFFFF_FFF0) as u64,
            prefetchable: p,
        },
        4 => {
            if index == 5 {
                return Bar::Unused;
            }
            let upper = dev.bars[index + 1] as u64;
            Bar::Memory {
                addr: (raw as u64 & 0xFFFF_FFF0) | (upper << 32),
                prefetchable: p,
            }
        }
        _ => Bar::Unused,
    }
}
