use alloc::vec::Vec;
use crate::acpi::platform::AcpiError;

fn sig(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

fn checksum(buf: &[u8]) -> bool {
    buf.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

fn map_region(paddr: u64, size: u64) -> u64 {
    let offset = paddr & 0xFFF;
    let aligned = paddr - offset;
    let total = size + offset;
    let pages = (total + 0xFFF) & !0xFFF;
    let vaddr = crate::acpi::map_device_mmio(aligned, pages,
        crate::mm::vmm::PageFlags::READ | crate::mm::vmm::PageFlags::WRITE);
    vaddr + offset
}

pub struct SdtEntry {
    pub signature: u32,
    pub vaddr: u64,
    pub phys_addr: u64,
    pub length: u32,
}

/// Parse ACPI tables from an already-mapped RSDP byte slice (embedded in
/// a Multiboot2 tag, for example).  Extracts the RSDT / XSDT address from
/// the RSDP data and walks the corresponding table.
pub fn parse_tables_from_data(rsdp_data: &[u8]) -> Result<Vec<SdtEntry>, AcpiError> {
    if rsdp_data.len() < 20 {
        return Err(AcpiError::BadSignature);
    }

    if &rsdp_data[..8] != b"RSD PTR " {
        return Err(AcpiError::BadSignature);
    }

    if !checksum(&rsdp_data[..20]) {
        return Err(AcpiError::BadChecksum);
    }

    let revision = rsdp_data[15];

    let rsdt_addr_u32 = u32::from_le_bytes([rsdp_data[16], rsdp_data[17], rsdp_data[18], rsdp_data[19]]);

    let length = if revision >= 2 {
        if rsdp_data.len() < 24 {
            return Err(AcpiError::BadSignature);
        }
        u32::from_le_bytes([rsdp_data[20], rsdp_data[21], rsdp_data[22], rsdp_data[23]])
    } else {
        20
    };

    let xsdt_addr_u64 = if revision >= 2 && rsdp_data.len() >= 32 {
        u64::from_le_bytes([
            rsdp_data[24], rsdp_data[25], rsdp_data[26], rsdp_data[27],
            rsdp_data[28], rsdp_data[29], rsdp_data[30], rsdp_data[31],
        ])
    } else {
        0
    };

    // Extended checksum for revision >= 2
    if revision >= 2 {
        let len = length as usize;
        if rsdp_data.len() < len {
            return Err(AcpiError::BadSignature);
        }
        if !checksum(&rsdp_data[..len]) {
            return Err(AcpiError::BadChecksum);
        }
    }

    if revision >= 2 && xsdt_addr_u64 != 0 {
        walk_xsdt(xsdt_addr_u64)
    } else if rsdt_addr_u32 != 0 {
        walk_rsdt(rsdt_addr_u32 as u64)
    } else {
        Err(AcpiError::TableNotFound)
    }
}

/// Parse ACPI tables by mapping the RSDP from physical memory.
///
/// Maps a minimum of 36 bytes, then re-maps with the full length reported
/// in the RSDP for ACPI 2.0+ before delegating to `parse_tables_from_data`.
pub fn parse_tables(rsdp_addr: u64) -> Result<Vec<SdtEntry>, AcpiError> {
    let rsdp_vaddr = map_region(rsdp_addr, 36);

    let raw = unsafe { core::slice::from_raw_parts(rsdp_vaddr as *const u8, 36) };
    if &raw[..8] != b"RSD PTR " {
        return Err(AcpiError::BadSignature);
    }
    if !checksum(&raw[..20]) {
        return Err(AcpiError::BadChecksum);
    }

    let revision = raw[15];
    let length = if revision >= 2 {
        u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]])
    } else {
        20
    };

    // Re-map with the full RSDP length for ACPI 2.0+ (needed for the extended
    // checksum and in case the initial 36 bytes were insufficient).
    let rsdp_data = if length as u64 > 36 {
        let vaddr = map_region(rsdp_addr, length as u64);
        unsafe { core::slice::from_raw_parts(vaddr as *const u8, length as usize) }
    } else {
        raw
    };

    parse_tables_from_data(rsdp_data)
}

fn walk_xsdt(xsdt_addr: u64) -> Result<Vec<SdtEntry>, AcpiError> {
    let vaddr = map_region(xsdt_addr, 8);
    let hdr_len = unsafe {
        let p = vaddr as *const u8;
        u32::from_le_bytes([*p.add(4), *p.add(5), *p.add(6), *p.add(7)])
    };
    let vaddr = map_region(xsdt_addr, hdr_len as u64);

    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, hdr_len as usize) };
    if !checksum(raw) {
        return Err(AcpiError::BadChecksum);
    }

    if raw[0..4] != [b'X', b'S', b'D', b'T'] {
        return Err(AcpiError::BadSignature);
    }

    let entry_count = (hdr_len as usize - 36) / 8;
    let entries_addr = vaddr + 36;

    let mut result = Vec::new();
    for i in 0..entry_count {
        // XSDT entries are at offset 36 from the table base; 36 is not a
        // multiple of 8 so the u64 may be misaligned.  Read byte-by-byte
        // to avoid panicking on alignment-check.
        let entry_raw = unsafe {
            let p = (entries_addr + (i * 8) as u64) as *const u8;
            (p.add(0).read_volatile() as u64)
                | (p.add(1).read_volatile() as u64) << 8
                | (p.add(2).read_volatile() as u64) << 16
                | (p.add(3).read_volatile() as u64) << 24
                | (p.add(4).read_volatile() as u64) << 32
                | (p.add(5).read_volatile() as u64) << 40
                | (p.add(6).read_volatile() as u64) << 48
                | (p.add(7).read_volatile() as u64) << 56
        };
        result.extend(map_sdt(entry_raw)?);
    }
    Ok(result)
}

fn walk_rsdt(rsdt_addr: u64) -> Result<Vec<SdtEntry>, AcpiError> {
    let vaddr = map_region(rsdt_addr, 8);
    let hdr_len = unsafe {
        let p = vaddr as *const u8;
        u32::from_le_bytes([*p.add(4), *p.add(5), *p.add(6), *p.add(7)])
    };
    let vaddr = map_region(rsdt_addr, hdr_len as u64);

    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, hdr_len as usize) };
    if !checksum(raw) {
        return Err(AcpiError::BadChecksum);
    }

    if raw[0..4] != [b'R', b'S', b'D', b'T'] {
        return Err(AcpiError::BadSignature);
    }

    let entry_count = (hdr_len as usize - 36) / 4;
    let entries_addr = vaddr + 36;

    let mut result = Vec::new();
    for i in 0..entry_count {
        let entry_raw = unsafe {
            let p = (entries_addr + (i * 4) as u64) as *const u32;
            p.read_volatile()
        };
        result.extend(map_sdt(entry_raw as u64)?);
    }
    Ok(result)
}

fn map_sdt(phys_addr: u64) -> Result<Option<SdtEntry>, AcpiError> {
    let vaddr = map_region(phys_addr, 8);
    let hdr_len = unsafe {
        let p = vaddr as *const u8;
        u32::from_le_bytes([*p.add(4), *p.add(5), *p.add(6), *p.add(7)])
    };

    let vaddr = map_region(phys_addr, hdr_len as u64);
    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, hdr_len as usize) };

    if !checksum(raw) {
        // warn and skip
        log::warn!("ACPI table at 0x{:x}: bad checksum, skipping", phys_addr);
        return Ok(None);
    }

    let sig_bytes: [u8; 4] = [raw[0], raw[1], raw[2], raw[3]];
    // Only care about tables we actually parse
    let sig_u32 = u32::from_le_bytes(sig_bytes);
    let keep_sigs = [sig(b"FACP"), sig(b"APIC"), sig(b"MCFG")];
    if !keep_sigs.contains(&sig_u32) {
        return Ok(None);
    }

    log::info!("ACPI: found table {:?} at 0x{:x} ({})",
        core::str::from_utf8(&sig_bytes).unwrap_or("????"), phys_addr, hdr_len);

    Ok(Some(SdtEntry {
        signature: sig_u32,
        vaddr,
        phys_addr,
        length: hdr_len,
    }))
}
