use alloc::string::String;
use alloc::vec::Vec;

use crate::filesystems::vfs::error::VfsError;

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_OBJECT_ID: u32 = 0x40;
pub const ATTR_SECURITY_DESCRIPTOR: u32 = 0x50;
pub const ATTR_VOLUME_NAME: u32 = 0x60;
pub const ATTR_VOLUME_INFORMATION: u32 = 0x70;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_INDEX_ROOT: u32 = 0x90;
pub const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
pub const ATTR_BITMAP: u32 = 0xB0;
pub const ATTR_REPARSE_POINT: u32 = 0xC0;
pub const ATTR_END: u32 = 0xFFFF_FFFF;

pub const FLAG_COMPRESSED: u16 = 0x0001;
pub const FLAG_ENCRYPTED: u16 = 0x4000;
pub const FLAG_SPARSE: u16 = 0x8000;

/// FILE_ATTRIBUTE_DIRECTORY, set in the $FILE_NAME flags of directories.
pub const FILE_NAME_INDEX_PRESENT: u32 = 0x1000_0000;

/// In an index entry, low 48 bits of the file reference are the MFT record.
pub const MFT_REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

const MAX_ATTRS: usize = 256;

#[inline]
pub(crate) fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    let s = d.get(o..o + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
pub(crate) fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    let s = d.get(o..o + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
pub(crate) fn u64_at(d: &[u8], o: usize) -> Option<u64> {
    let s = d.get(o..o + 8)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

pub(crate) fn decode_utf16le(units: &[u8]) -> String {
    let words: Vec<u16> = units
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&words)
}

/// A parsed attribute header referencing its owning record's data.  For
/// resident attributes `value` is the attribute value; for non-resident
/// attributes `map_off` is the absolute offset of the mapping pairs and
/// `map_end` the absolute end of the attribute (pairs must stay inside it).
pub struct Attr<'a> {
    pub attr_type: u32,
    pub name: Option<String>,
    pub flags: u16,
    pub resident: bool,
    pub value: &'a [u8],
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    pub map_off: usize,
    pub map_end: usize,
    pub real_size: u64,
    pub init_size: u64,
}

/// Walk all attributes of an MFT record.  Every read is bounds-checked
/// against the record's allocated size; a malformed record yields an error,
/// never a panic.
pub(crate) fn iter_attrs(record: &[u8]) -> Result<Vec<Attr<'_>>, VfsError> {
    let mut out = Vec::new();
    if record.len() < 0x2C {
        return Err(VfsError::IOError);
    }
    let first = u16_at(record, 0x14).ok_or(VfsError::IOError)? as usize;
    let alloc = u32_at(record, 0x1C).ok_or(VfsError::IOError)? as usize;
    let end = alloc.min(record.len());
    if first < 0x18 || first >= end {
        return Err(VfsError::IOError);
    }

    let mut pos = first;
    while pos + 8 <= end {
        if out.len() >= MAX_ATTRS {
            return Err(VfsError::IOError);
        }
        let t = u32_at(record, pos).ok_or(VfsError::IOError)?;
        if t == ATTR_END {
            break;
        }
        let len = u32_at(record, pos + 4).ok_or(VfsError::IOError)? as usize;
        if len < 0x18 || pos + len > end {
            return Err(VfsError::IOError);
        }
        let nonresident = record[pos + 0x08] != 0;
        let name_len = record[pos + 0x09] as usize;
        let name_off = u16_at(record, pos + 0x0A).ok_or(VfsError::IOError)? as usize;
        let flags = u16_at(record, pos + 0x0C).ok_or(VfsError::IOError)?;

        let name = if name_len == 0 {
            None
        } else {
            if name_off + name_len * 2 > len {
                return Err(VfsError::IOError);
            }
            Some(decode_utf16le(
                &record[pos + name_off..pos + name_off + name_len * 2],
            ))
        };

        if nonresident {
            if len < 0x40 {
                return Err(VfsError::IOError);
            }
            let lowest_vcn = u64_at(record, pos + 0x10).ok_or(VfsError::IOError)?;
            let highest_vcn = u64_at(record, pos + 0x18).ok_or(VfsError::IOError)?;
            let map_off = u16_at(record, pos + 0x20).ok_or(VfsError::IOError)? as usize;
            let real_size = u64_at(record, pos + 0x30).ok_or(VfsError::IOError)?;
            let init_size = u64_at(record, pos + 0x38).ok_or(VfsError::IOError)?;
            if map_off < 0x40 || map_off >= len {
                return Err(VfsError::IOError);
            }
            out.push(Attr {
                attr_type: t,
                name,
                flags,
                resident: false,
                value: &[],
                lowest_vcn,
                highest_vcn,
                map_off: pos + map_off,
                map_end: pos + len,
                real_size,
                init_size,
            });
        } else {
            let vlen = u32_at(record, pos + 0x10).ok_or(VfsError::IOError)? as usize;
            let voff = u16_at(record, pos + 0x14).ok_or(VfsError::IOError)? as usize;
            if voff + vlen > len {
                return Err(VfsError::IOError);
            }
            out.push(Attr {
                attr_type: t,
                name,
                flags,
                resident: true,
                value: &record[pos + voff..pos + voff + vlen],
                lowest_vcn: 0,
                highest_vcn: 0,
                map_off: 0,
                map_end: 0,
                real_size: vlen as u64,
                init_size: vlen as u64,
            });
        }
        pos += len;
    }

    Ok(out)
}

/// Decoded $FILE_NAME attribute value (also the key of a directory index).
pub struct FileName {
    pub parent_ref: u64,
    pub mtime: u64,
    pub size: u64,
    pub flags: u32,
    pub namespace: u8,
    pub name: String,
}

pub(crate) fn parse_file_name(value: &[u8]) -> Result<FileName, VfsError> {
    if value.len() < 0x42 {
        return Err(VfsError::IOError);
    }
    let parent_ref = u64_at(value, 0x00).ok_or(VfsError::IOError)?;
    let mtime_ft = u64_at(value, 0x10).ok_or(VfsError::IOError)?;
    let size = u64_at(value, 0x30).ok_or(VfsError::IOError)?;
    let flags = u32_at(value, 0x38).ok_or(VfsError::IOError)?;
    let name_len = value[0x40] as usize;
    let namespace = value[0x41];
    if 0x42 + name_len * 2 > value.len() {
        return Err(VfsError::IOError);
    }
    let name = decode_utf16le(&value[0x42..0x42 + name_len * 2]);
    Ok(FileName {
        parent_ref,
        mtime: filetime_to_epoch(mtime_ft),
        size,
        flags,
        namespace,
        name,
    })
}

/// Windows FILETIME (100 ns since 1601-01-01) -> Unix epoch seconds.
pub(crate) fn filetime_to_epoch(ft: u64) -> u64 {
    (ft / 10_000_000).saturating_sub(116_444_736_00)
}

/// One entry of an $ATTRIBUTE_LIST (attributes that overflow their base
/// record live in extension records referenced here).
pub struct AttrListEntry {
    pub attr_type: u32,
    pub name: Option<String>,
    pub lowest_vcn: u64,
    pub mft_ref: u64,
}

pub(crate) fn parse_attr_list(value: &[u8]) -> Result<Vec<AttrListEntry>, VfsError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 0x18 <= value.len() {
        if out.len() >= MAX_ATTRS {
            return Err(VfsError::IOError);
        }
        let t = u32_at(value, pos).ok_or(VfsError::IOError)?;
        if t == ATTR_END {
            break;
        }
        let len = u16_at(value, pos + 4).ok_or(VfsError::IOError)? as usize;
        if len < 0x1A || pos + len > value.len() {
            return Err(VfsError::IOError);
        }
        let name_len = value[pos + 6] as usize;
        let name_off = value[pos + 7] as usize;
        let lowest_vcn = u64_at(value, pos + 8).ok_or(VfsError::IOError)?;
        let mft_ref = u64_at(value, pos + 0x10).ok_or(VfsError::IOError)?;
        let name = if name_len == 0 {
            None
        } else {
            if name_off + name_len * 2 > len {
                return Err(VfsError::IOError);
            }
            Some(decode_utf16le(
                &value[pos + name_off..pos + name_off + name_len * 2],
            ))
        };
        out.push(AttrListEntry {
            attr_type: t,
            name,
            lowest_vcn,
            mft_ref,
        });
        pos += len;
    }
    Ok(out)
}

/// Read a (possibly non-resident) attribute's value into a fresh buffer,
/// capped to prevent a malicious size from allocating absurd amounts.
pub(crate) fn read_attr_value<'a>(
    device: &dyn crate::filesystems::blockdriver::traits::BlockDevice,
    boot: &super::boot::BootSector,
    record: &'a [u8],
    attr: &Attr<'a>,
    cap: u64,
) -> Result<Vec<u8>, VfsError> {
    if attr.resident {
        return Ok(attr.value.to_vec());
    }
    let pairs = record
        .get(attr.map_off..attr.map_end)
        .ok_or(VfsError::IOError)?;
    let runs = super::runs::decode_mapping_pairs(pairs, attr.lowest_vcn)?;
    let want = core::cmp::min(attr.real_size, cap);
    let mut buf = alloc::vec![0u8; want as usize];
    super::runs::read_file_at(device, boot, &runs, 0, &mut buf)?;
    Ok(buf)
}
