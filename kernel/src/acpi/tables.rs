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
    pub length: u32,
}

pub fn parse_tables(rsdp_addr: u64) -> Result<Vec<SdtEntry>, AcpiError> {
    let rsdp_vaddr = map_region(rsdp_addr, 36);

    let sig_arr: [u8; 8] = unsafe { (rsdp_vaddr as *const [u8; 8]).read() };
    if &sig_arr != b"RSD PTR " {
        return Err(AcpiError::BadSignature);
    }

    let raw = unsafe { core::slice::from_raw_parts(rsdp_vaddr as *const u8, 36) };
    if !checksum(&raw[..20]) {
        return Err(AcpiError::BadChecksum);
    }

    let revision = raw[15];

    let rsdt_addr_u32 = u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]);

    // For ACPI 2.0+:
    let length = if revision >= 2 { u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]) } else { 20 };
    let xsdt_addr_u64 = if revision >= 2 {
        u64::from_le_bytes([raw[24], raw[25], raw[26], raw[27], raw[28], raw[29], raw[30], raw[31]])
    } else {
        0
    };

    // Extended checksum for revision >= 2
    if revision >= 2 {
        let ext_raw = unsafe { core::slice::from_raw_parts(rsdp_vaddr as *const u8, length as usize) };
        if !checksum(&ext_raw[0..length as usize]) {
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
        length: hdr_len,
    }))
}
