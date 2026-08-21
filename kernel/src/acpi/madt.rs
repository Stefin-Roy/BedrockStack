use crate::acpi::platform::{
    AcpiError, Apic, InterruptModel, IoApic, Polarity, Processor, ProcessorInfo, ProcessorState,
    TriggerMode,
};
use crate::drivers::serial::SerialPort;
use alloc::vec::Vec;
use log::info;
use spin::Mutex;

fn r8(buf: &[u8], off: usize) -> u8 {
    buf[off]
}
fn r16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn r32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn r64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

/// A MADT Interrupt Source Override (type 2) entry.
#[derive(Clone, Copy, Debug)]
pub struct IrqOverride {
    /// Legacy ISA IRQ number (e.g. 1 for the keyboard).
    pub irq: u8,
    /// GSI that the legacy IRQ is actually wired to.
    pub gsi: u32,
    pub polarity: Polarity,
    pub trigger: TriggerMode,
}

static OVERRIDES: Mutex<Vec<IrqOverride>> = Mutex::new(Vec::new());

/// Look up the MADT interrupt source override for a legacy ISA IRQ.
///
/// Returns `Some((gsi, polarity, trigger))` when an override exists, `None`
/// when the firmware relies on the default ISA wiring (IRQ n → GSI n,
/// ActiveHigh, Edge).
pub fn irq_override(irq: u8) -> Option<(u32, Polarity, TriggerMode)> {
    OVERRIDES
        .lock()
        .iter()
        .find(|o| o.irq == irq)
        .map(|o| (o.gsi, o.polarity, o.trigger))
}

pub fn parse_madt(
    vaddr: u64,
    phys_addr: u64,
    length: u32,
) -> Result<(InterruptModel, Option<ProcessorInfo>), AcpiError> {
    let initial_raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, length as usize) };

    SerialPort::puts("[madt] parse_madt called: vaddr=");
    SerialPort::put_hex(vaddr);
    SerialPort::puts(" phys=");
    SerialPort::put_hex(phys_addr);
    SerialPort::puts(" length=");
    SerialPort::put_u64(length as u64);
    SerialPort::puts(" first_bytes=");
    for i in 0..4 {
        SerialPort::put_hex(initial_raw[i] as u64);
        SerialPort::puts(" ");
    }
    SerialPort::puts("\n");

    let raw: &[u8];
    let mut _fallback_buf = alloc::vec::Vec::new();

    if initial_raw[0..4] != [b'A', b'P', b'I', b'C'] {
        SerialPort::puts("[madt] BAD SIGNATURE at vaddr, re-mapping from phys 0x");
        SerialPort::put_hex(phys_addr);
        SerialPort::puts("\n");

        let offset = phys_addr & 0xFFF;
        let aligned = phys_addr - offset;
        let pages = ((length as u64) + offset + 0xFFF) & !0xFFF;
        let new_vaddr = crate::acpi::try_map_device_mmio(
            aligned,
            pages,
            crate::mm::vmm::PageFlags::READ | crate::mm::vmm::PageFlags::WRITE,
        )
        .map(|v| v + offset)
        .unwrap_or_else(|e| {
            log::error!("MADT re-map failed: {} (phys={:#x})", e, phys_addr);
            // Return original vaddr to allow graceful error below, not panic.
            vaddr
        });

        SerialPort::puts("[madt] re-mapped to vaddr=");
        SerialPort::put_hex(new_vaddr);
        SerialPort::puts("\n");

        let new_raw =
            unsafe { core::slice::from_raw_parts(new_vaddr as *const u8, length as usize) };

        SerialPort::puts("[madt] re-mapped first_bytes=");
        for i in 0..4 {
            SerialPort::put_hex(new_raw[i] as u64);
            SerialPort::puts(" ");
        }
        SerialPort::puts("\n");

        if new_raw[0..4] != [b'A', b'P', b'I', b'C'] {
            SerialPort::puts("[madt] BAD SIGNATURE after re-map too, returning Err\n");
            return Err(AcpiError::BadSignature);
        }

        SerialPort::puts("[madt] re-map OK, using fallback buffer\n");
        _fallback_buf.extend_from_slice(new_raw);
        raw = &_fallback_buf;
    } else {
        raw = initial_raw;
    }

    let mut local_apic_address = r32(raw, 36) as u64;
    let _flags = r32(raw, 40);

    let mut io_apics: Vec<IoApic> = Vec::new();
    let mut xapic_processors: Vec<Processor> = Vec::new();
    let mut x2apic_processors: Vec<Processor> = Vec::new();
    let mut has_boot_xapic = false;
    let mut has_boot_x2apic = false;

    let mut offset = 44;
    let mut entry_count = 0u32;
    while offset + 2 <= length as usize {
        let entry_type = r8(raw, offset);
        let entry_len = r8(raw, offset + 1);
        entry_count += 1;
        SerialPort::puts("[madt] entry ");
        SerialPort::put_u64(entry_count as u64);
        SerialPort::puts(": type=");
        SerialPort::put_u64(entry_type as u64);
        SerialPort::puts(" len=");
        SerialPort::put_u64(entry_len as u64);
        SerialPort::puts("\n");

        if entry_len < 2 || offset + entry_len as usize > length as usize {
            SerialPort::puts("[madt] entry bounds check failed, breaking\n");
            break;
        }

        match entry_type {
            0x0 => {
                // Local APIC entry (8 bytes)
                //   offset+2 = ACPI Processor UID
                //   offset+3 = APIC ID
                if entry_len >= 8 {
                    let apic_id = r8(raw, offset + 3);
                    let flags = r32(raw, offset + 4);
                    let enabled = (flags & 1) != 0;
                    let state = if enabled {
                        ProcessorState::Enabled
                    } else {
                        ProcessorState::Disabled
                    };
                    info!(
                        "[madt] type 0 (Local APIC): apic_id={} enabled={}",
                        apic_id, enabled
                    );
                    xapic_processors.push(Processor {
                        local_apic_id: apic_id as u32,
                        state,
                        is_ap: has_boot_xapic,
                    });
                    has_boot_xapic = true;
                }
            }
            0x9 => {
                // Processor Local x2APIC entry (16 bytes)
                //   offset+2 = 2 bytes reserved
                //   offset+4 = 4 bytes x2APIC ID (u32, little-endian)
                //   offset+8 = 4 bytes flags
                if entry_len >= 16 {
                    let apic_id = r32(raw, offset + 4);
                    let flags = r32(raw, offset + 8);
                    let enabled = (flags & 1) != 0;
                    let state = if enabled {
                        ProcessorState::Enabled
                    } else {
                        ProcessorState::Disabled
                    };
                    info!(
                        "[madt] type 9 (x2APIC): apic_id={} enabled={}",
                        apic_id, enabled
                    );
                    x2apic_processors.push(Processor {
                        local_apic_id: apic_id,
                        state,
                        is_ap: has_boot_x2apic,
                    });
                    has_boot_x2apic = true;
                }
            }
            0x1 => {
                // I/O APIC entry (12 bytes)
                if entry_len >= 12 {
                    let io_apic_address = r32(raw, offset + 4);
                    let gsi_base = r32(raw, offset + 8);
                    io_apics.push(IoApic {
                        address: io_apic_address as u64,
                        global_system_interrupt_base: gsi_base,
                    });
                }
            }
            0x2 => {
                // Interrupt Source Override (10 bytes).  Legacy IRQs are
                // remapped onto GSIs here (e.g. keyboard IRQ 1 → GSI 9 on
                // hardware where the ISA interrupt lines are redirected).
                if entry_len >= 10 {
                    let bus = r8(raw, offset + 2);
                    let source = r8(raw, offset + 3);
                    if bus == 0 {
                        let gsi = r32(raw, offset + 4);
                        let flags = r16(raw, offset + 8);
                        let polarity = match flags & 0x3 {
                            0 | 1 => Polarity::ActiveHigh,
                            _ => Polarity::ActiveLow,
                        };
                        let trigger = match (flags >> 2) & 0x3 {
                            0 | 1 => TriggerMode::Edge,
                            _ => TriggerMode::Level,
                        };
                        OVERRIDES.lock().push(IrqOverride {
                            irq: source,
                            gsi,
                            polarity,
                            trigger,
                        });
                        SerialPort::puts("[madt] type 2 (IRQ override): irq=");
                        SerialPort::put_u64(source as u64);
                        SerialPort::puts(" gsi=");
                        SerialPort::put_u64(gsi as u64);
                        SerialPort::puts("\n");
                    }
                }
            }
            0x5 => {
                // Local APIC address override (16 bytes)
                if entry_len >= 16 {
                    local_apic_address = r64(raw, offset + 4);
                }
            }
            _ => {}
        }

        offset += entry_len as usize;
    }

    SerialPort::puts("[madt] entries processed: ");
    SerialPort::put_u64(entry_count as u64);
    SerialPort::puts(" xapic=");
    SerialPort::put_u64(xapic_processors.len() as u64);
    SerialPort::puts(" x2apic=");
    SerialPort::put_u64(x2apic_processors.len() as u64);
    SerialPort::puts(" ioapics=");
    SerialPort::put_u64(io_apics.len() as u64);
    SerialPort::puts("\n");

    let model = InterruptModel::Apic(Apic {
        io_apics,
        local_apic_address,
    });

    // Driver policy: type-9 x2APIC entries carry the authoritative 32-bit
    // APIC IDs when present.  Some firmware additionally emits type-0 xAPIC
    // entries as legacy compatibility stubs with APIC ID 0.  Prefer the
    // x2APIC list only when it actually carries a nonzero ID; otherwise fall
    // back to the xAPIC entries rather than discarding valid processors.
    let x2apic_valid =
        !x2apic_processors.is_empty() && x2apic_processors.iter().any(|p| p.local_apic_id != 0);
    let mut processors = if x2apic_valid {
        info!("[madt] using x2APIC (type 9) processor entries");
        x2apic_processors
    } else {
        info!("[madt] using xAPIC (type 0) processor entries");
        xapic_processors
    };

    let processor_info = if processors.is_empty() {
        None
    } else {
        let boot = processors.remove(0);
        Some(ProcessorInfo {
            boot_processor: boot,
            application_processors: processors,
        })
    };

    Ok((model, processor_info))
}
