use super::PciDevice;

/// Decoded PCI BAR.
#[derive(Debug, Clone, Copy)]
pub enum Bar {
    /// Slot is not present (out of range, consumed by preceding 64-bit BAR,
    /// or reserved encoding).
    Unused,

    /// Memory-mapped I/O BAR.
    Memory {
        addr: u64,
        prefetchable: bool,
    },

    /// Port I/O BAR.
    Io {
        port: u32,
    },
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
        0 => Bar::Memory { addr: (raw & 0xFFFF_FFF0) as u64, prefetchable: p },
        4 => {
            if index == 5 {
                return Bar::Unused;
            }
            let upper = dev.bars[index + 1] as u64;
            Bar::Memory { addr: (raw as u64 & 0xFFFF_FFF0) | (upper << 32), prefetchable: p }
        }
        _ => Bar::Unused,
    }
}
