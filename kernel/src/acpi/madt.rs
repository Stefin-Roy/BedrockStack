use alloc::vec::Vec;
use crate::acpi::platform::{AcpiError, Apic, InterruptModel, IoApic, Processor, ProcessorInfo, ProcessorState};
use log::info;

fn r8(buf: &[u8], off: usize) -> u8 { buf[off] }
fn r32(buf: &[u8], off: usize) -> u32 { u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) }
fn r64(buf: &[u8], off: usize) -> u64 { u64::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3], buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]) }

pub fn parse_madt(vaddr: u64, length: u32) -> Result<(InterruptModel, Option<ProcessorInfo>), AcpiError> {
    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, length as usize) };

    if raw[0..4] != [b'A', b'P', b'I', b'C'] {
        return Err(AcpiError::BadSignature);
    }

    let mut local_apic_address = r32(raw, 36) as u64;
    let _flags = r32(raw, 40);

    let mut io_apics: Vec<IoApic> = Vec::new();
    let mut xapic_processors: Vec<Processor> = Vec::new();
    let mut x2apic_processors: Vec<Processor> = Vec::new();
    let mut has_boot_xapic = false;
    let mut has_boot_x2apic = false;

    let mut offset = 44;
    while offset + 2 <= length as usize {
        let entry_type = r8(raw, offset);
        let entry_len = r8(raw, offset + 1);
        if entry_len < 2 || offset + entry_len as usize > length as usize {
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
                    let state = if enabled { ProcessorState::Enabled } else { ProcessorState::Disabled };
                    info!("[madt] type 0 (Local APIC): apic_id={} enabled={}", apic_id, enabled);
                    xapic_processors.push(Processor { local_apic_id: apic_id as u32, state, is_ap: has_boot_xapic });
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
                    let state = if enabled { ProcessorState::Enabled } else { ProcessorState::Disabled };
                    info!("[madt] type 9 (x2APIC): apic_id={} enabled={}", apic_id, enabled);
                    x2apic_processors.push(Processor { local_apic_id: apic_id, state, is_ap: has_boot_x2apic });
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

    let model = InterruptModel::Apic(Apic {
        io_apics,
        local_apic_address,
    });

    // Prefer x2APIC entries when present. On x2APIC-capable firmware the type 0
    // entries are emitted as legacy compatibility stubs with APIC ID 0, so the
    // authoritative IDs live in the type 9 entries.
    let mut processors = if !x2apic_processors.is_empty() {
        info!("[madt] using x2APIC (type 9) processor entries");
        x2apic_processors
    } else {
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
