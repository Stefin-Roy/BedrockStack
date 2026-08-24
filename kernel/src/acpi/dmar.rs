use crate::acpi::platform::{AcpiError, Atsr, DeviceScope, DmarInfo, Drhd, Rmrr};
use crate::drivers::serial::SerialPort;

fn r8(buf: &[u8], off: usize) -> u8 {
    buf[off]
}
fn r16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
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

fn parse_device_scope(buf: &[u8], off: usize, scope_len: usize) -> Option<DeviceScope> {
    if scope_len < 6 || off + scope_len > buf.len() {
        return None;
    }
    let device_type = r8(buf, off);
    let len = r8(buf, off + 1);
    if len as usize != scope_len {
        return None;
    }
    // Spec p8-4: Type@0 Len@1 Flags@2 Reserved@3 EnumID@4 StartBus@5 Path@6
    let _flags = r8(buf, off + 2);
    let _reserved = r8(buf, off + 3);
    let enum_id = r8(buf, off + 4);
    let start_bus = r8(buf, off + 5);
    let path_len = scope_len.saturating_sub(6);
    if path_len % 2 != 0 {
        return None;
    }
    let mut path = alloc::vec::Vec::new();
    let mut p = off + 6;
    let end = off + scope_len;
    while p + 1 < end {
        let dev = r8(buf, p);
        let fun = r8(buf, p + 1);
        path.push((dev, fun));
        p += 2;
    }
    Some(DeviceScope {
        device_type,
        enumeration_id: enum_id,
        start_bus_number: start_bus,
        path,
    })
}

pub fn parse_dmar(vaddr: u64, length: u32) -> Result<DmarInfo, AcpiError> {
    if length < 48 {
        SerialPort::puts("[dmar] length < 48\n");
        return Err(AcpiError::InvalidData);
    }
    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, length as usize) };
    if raw[0..4] != [b'D', b'M', b'A', b'R'] {
        SerialPort::puts("[dmar] bad signature\n");
        return Err(AcpiError::BadSignature);
    }

    let host_address_width = r8(raw, 36);
    let flags = r8(raw, 37);
    // bytes 38..48 reserved (10 bytes) must be zero per spec, but tolerant.

    if host_address_width == 0 || host_address_width > 64 {
        log::warn!(
            "[dmar] host_address_width {} out of range, clamping skipped",
            host_address_width
        );
        // Do not fail — firmware may report 0 on some QEMU versions; treat as 39 default?
        // But we still propagate; caller may ignore. Require plausible.
        if host_address_width > 64 {
            return Err(AcpiError::InvalidData);
        }
    }

    let mut drhds = alloc::vec::Vec::new();
    let mut rmrrs = alloc::vec::Vec::new();
    let mut atsr = alloc::vec::Vec::new();

    let mut offset = 48usize;
    let total = length as usize;
    while offset + 4 <= total {
        let struct_type = r16(raw, offset);
        let struct_len = r16(raw, offset + 2) as usize;
        if struct_len < 4 || offset + struct_len > total {
            SerialPort::puts("[dmar] entry len bounds fail\n");
            log::warn!(
                "[dmar] entry at {} type {} len {} OOB (total {})",
                offset,
                struct_type,
                struct_len,
                total
            );
            break;
        }

        match struct_type {
            0 => {
                // DRHD spec p8-3: Flags@4 Size@5 Segment@6 RegBase@8(8) Scope@16
                if struct_len < 16 {
                    log::warn!("[dmar] DRHD len < 16, skip");
                } else {
                    let flags_drhd = r8(raw, offset + 4);
                    let size_drhd = r8(raw, offset + 5);
                    let segment = r16(raw, offset + 6);
                    let reg_base = r64(raw, offset + 8);
                    let include_all = (flags_drhd & 1) != 0;
                    let mut devices = alloc::vec::Vec::new();
                    let mut scope_off = offset + 16;
                    let end = offset + struct_len;
                    while scope_off + 2 <= end {
                        let s_len = r8(raw, scope_off + 1) as usize;
                        if s_len < 6 || scope_off + s_len > end {
                            log::warn!("[dmar] DRHD scope OOB");
                            break;
                        }
                        if let Some(ds) = parse_device_scope(raw, scope_off, s_len) {
                            devices.push(ds);
                        }
                        scope_off += s_len;
                    }
                    SerialPort::puts("[dmar] DRHD seg=");
                    SerialPort::put_hex(segment as u64);
                    SerialPort::puts(" base=");
                    SerialPort::put_hex(reg_base);
                    SerialPort::puts(" flags=");
                    SerialPort::put_hex(flags_drhd as u64);
                    SerialPort::puts(" incl_all=");
                    SerialPort::put_u64(include_all as u64);
                    SerialPort::puts(" dev_scopes=");
                    SerialPort::put_u64(devices.len() as u64);
                    SerialPort::puts("\n");
                    drhds.push(Drhd {
                        flags: flags_drhd,
                        segment,
                        register_base: reg_base,
                        include_pci_all: include_all,
                        devices,
                    });
                    let _ = size_drhd;
                }
            }
            1 => {
                // RMRR spec p8-11: Reserved@4(2) Segment@6(2) Base@8(8) Limit@16(8) Scope@24
                if struct_len < 24 {
                    log::warn!("[dmar] RMRR len < 24, skip");
                } else {
                    let _rsvd = r16(raw, offset + 4);
                    let segment = r16(raw, offset + 6);
                    let base = r64(raw, offset + 8);
                    let limit = r64(raw, offset + 16);
                    let mut devices = alloc::vec::Vec::new();
                    let mut scope_off = offset + 24;
                    let end = offset + struct_len;
                    while scope_off + 2 <= end {
                        let s_len = r8(raw, scope_off + 1) as usize;
                        if s_len < 6 || scope_off + s_len > end {
                            log::warn!("[dmar] RMRR scope OOB");
                            break;
                        }
                        if let Some(ds) = parse_device_scope(raw, scope_off, s_len) {
                            devices.push(ds);
                        }
                        scope_off += s_len;
                    }
                    SerialPort::puts("[dmar] RMRR seg=");
                    SerialPort::put_hex(segment as u64);
                    SerialPort::puts(" base=");
                    SerialPort::put_hex(base);
                    SerialPort::puts(" limit=");
                    SerialPort::put_hex(limit);
                    SerialPort::puts("\n");
                    // Sanity: base <= limit ?
                    if limit >= base {
                        rmrrs.push(Rmrr {
                            segment,
                            base_address: base,
                            limit_address: limit,
                            devices,
                        });
                    } else {
                        log::warn!("[dmar] RMRR base > limit, ignored");
                    }
                }
            }
            2 => {
                // ATSR - Root Port ATS Capability
                if struct_len < 8 {
                    log::warn!("[dmar] ATSR len < 8");
                } else {
                    let flags_a = r8(raw, offset + 4);
                    let segment = r16(raw, offset + 6);
                    let mut devices = alloc::vec::Vec::new();
                    let mut scope_off = offset + 8;
                    let end = offset + struct_len;
                    while scope_off + 2 <= end {
                        let s_len = r8(raw, scope_off + 1) as usize;
                        if s_len < 6 || scope_off + s_len > end {
                            break;
                        }
                        if let Some(ds) = parse_device_scope(raw, scope_off, s_len) {
                            devices.push(ds);
                        }
                        scope_off += s_len;
                    }
                    SerialPort::puts("[dmar] ATSR seg=");
                    SerialPort::put_hex(segment as u64);
                    SerialPort::puts(" flags=");
                    SerialPort::put_hex(flags_a as u64);
                    SerialPort::puts("\n");
                    atsr.push(Atsr {
                        flags: flags_a,
                        segment,
                        devices,
                    });
                }
            }
            3 => {
                // RHSA - Remapping Hardware Status Affinity
                SerialPort::puts("[dmar] RHSA ignored\n");
            }
            4 => {
                // ANDD - ACPI Name-space Device Declaration
                SerialPort::puts("[dmar] ANDD ignored\n");
            }
            _ => {
                SerialPort::puts("[dmar] unknown type ");
                SerialPort::put_u64(struct_type as u64);
                SerialPort::puts(" len=");
                SerialPort::put_u64(struct_len as u64);
                SerialPort::puts("\n");
            }
        }
        offset += struct_len;
    }

    SerialPort::puts("[dmar] parse done drhd=");
    SerialPort::put_u64(drhds.len() as u64);
    SerialPort::puts(" rmrr=");
    SerialPort::put_u64(rmrrs.len() as u64);
    SerialPort::puts(" atsr=");
    SerialPort::put_u64(atsr.len() as u64);
    SerialPort::puts("\n");

    Ok(DmarInfo {
        host_address_width,
        flags,
        drhds,
        rmrrs,
        atsr,
    })
}
