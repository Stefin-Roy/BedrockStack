//! Extraction of the `\_S5` (soft-off) sleep state from the DSDT AML.
//!
//! The SLP_TYP value for S5 is defined by the `\_S5` package in the DSDT.
//! This module performs a bounded byte-walk of the AML stream looking for
//! `NameOp ("_S5") PkgOp ...` and decodes the first package element
//! (SLP_TYPa).  It is deliberately not an AML interpreter: any construct it
//! cannot decode (method calls, aliases) yields `None`, and the caller must
//! then refuse to program a guessed sleep type rather than writing a wrong
//! one.

fn map_region(paddr: u64, size: u64) -> u64 {
    let offset = paddr & 0xFFF;
    let aligned = paddr - offset;
    let total = size + offset;
    let pages = (total + 0xFFF) & !0xFFF;
    let vaddr = crate::acpi::map_device_mmio(
        aligned,
        pages,
        crate::mm::vmm::PageFlags::READ | crate::mm::vmm::PageFlags::WRITE,
    );
    vaddr + offset
}

fn checksum(buf: &[u8]) -> bool {
    buf.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

/// Decode an AML package length (ACPI spec, PkgLength encoding) and advance
/// `off` past the encoded length.  Returns `None` on a truncated stream.
fn parse_pkg_len(aml: &[u8], off: &mut usize) -> Option<usize> {
    let byte0 = *aml.get(*off)?;
    *off += 1;
    if byte0 & 0x80 == 0 {
        return Some((byte0 & 0x7F) as usize);
    }
    let extra = (byte0 & 0x0F) as usize;
    let mut len = ((byte0 & 0x70) >> 4) as usize;
    for _ in 0..extra {
        let b = *aml.get(*off)? as usize;
        *off += 1;
        len = (len << 8) | b;
    }
    Some(len)
}

/// Decode a single AML integer constant as a `u8`.  Only the constant ops
/// are supported; anything else is not statically decodable.
fn decode_const(aml: &[u8], off: &mut usize, end: usize) -> Option<u8> {
    if *off >= end {
        return None;
    }
    match aml[*off] {
        0x00 => { *off += 1; Some(0) }   // ZeroOp
        0x01 => { *off += 1; Some(1) }   // OneOp
        0x0A => {                        // ByteConst
            *off += 1;
            let v = *aml.get(*off)?;
            *off += 1;
            Some(v)
        }
        0x0B => {                        // WordConst
            *off += 1;
            let lo = *aml.get(*off)?;
            let hi = *aml.get(*off + 1)?;
            *off += 2;
            Some((u16::from_le_bytes([lo, hi]) & 0x7) as u8)
        }
        0x0C => {                        // DWordConst
            *off += 1;
            let mut bytes = [0u8; 4];
            for b in bytes.iter_mut() {
                *b = *aml.get(*off)?;
                *off += 1;
            }
            Some((u32::from_le_bytes(bytes) & 0x7) as u8)
        }
        _ => None,
    }
}

/// Scan `aml` for the `\_S5` package and return SLP_TYPa.
fn scan_s5_slp_typa(aml: &[u8]) -> Option<u8> {
    let mut i = 0usize;
    while i + 6 <= aml.len() {
        // NameOp (0x08) + "_S5" + PkgOp (0x12)
        if aml[i] == 0x08
            && aml[i + 1] == 0x5F
            && aml[i + 2] == 0x53
            && aml[i + 3] == 0x35
            && aml[i + 4] == 0x5F
            && aml[i + 5] == 0x12
        {
            let mut off = i + 6;
            let pkg_len = parse_pkg_len(aml, &mut off)?;
            let end = off.checked_add(pkg_len)?;
            if end > aml.len() {
                return None;
            }
            return decode_const(aml, &mut off, end);
        }
        i += 1;
    }
    None
}

/// Map the DSDT from its physical address and extract the S5 SLP_TYPa value.
/// Returns `None` when the DSDT is missing, malformed, or the package cannot
/// be statically decoded — the caller must then skip the ACPI PM1 shutdown.
pub fn parse_s5_slp_typa(dsdt_phys: u64) -> Option<u8> {
    let vaddr = map_region(dsdt_phys, 8);
    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, 8) };
    if &raw[0..4] != b"DSDT" {
        log::warn!("ACPI: FADT DSDT pointer is not a DSDT signature");
        return None;
    }

    let hdr_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    if hdr_len < 36 || hdr_len > 0x0100_0000 {
        log::warn!("ACPI: DSDT invalid header length {}", hdr_len);
        return None;
    }

    let vaddr = map_region(dsdt_phys, hdr_len as u64);
    let table = unsafe { core::slice::from_raw_parts(vaddr as *const u8, hdr_len as usize) };
    if !checksum(table) {
        log::warn!("ACPI: DSDT bad checksum -- cannot parse \\_S5");
        return None;
    }

    scan_s5_slp_typa(table)
}
