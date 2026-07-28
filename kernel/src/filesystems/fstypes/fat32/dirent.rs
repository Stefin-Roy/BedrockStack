use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use hashbrown::HashSet;

use super::bpb::{DIR_ENTRY_SIZE, MAX_SFN_LEN};

pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LONG_NAME: u8 = 0x0F;
pub const DIR_DELETED: u8 = 0xE5;
pub const DIR_END: u8 = 0x00;

#[derive(Clone)]
pub(crate) struct DirEntrySlot {
    pub vfat_entries: Vec<[u8; DIR_ENTRY_SIZE]>,
    pub sfn_entry: [u8; DIR_ENTRY_SIZE],
}

pub fn set_timestamps(entry: &mut [u8; DIR_ENTRY_SIZE]) {
    entry[0x0D] = 0;
    entry[0x0E..0x10].copy_from_slice(&[0, 0]);
    entry[0x10..0x12].copy_from_slice(&[0, 0]);
    entry[0x12..0x14].copy_from_slice(&[0, 0]);
    entry[0x16..0x18].copy_from_slice(&[0, 0]);
    entry[0x18..0x1A].copy_from_slice(&[0, 0]);
}

pub fn decode_sfn(sfn: &[u8; MAX_SFN_LEN]) -> String {
    let mut name = String::new();
    let stem_end = sfn[..8].iter().rposition(|&b| b != b' ').map(|p| p + 1).unwrap_or(0);
    for b in &sfn[..stem_end] {
        name.push((*b as char).to_ascii_lowercase());
    }
    let ext_start = sfn[8..11].iter().position(|&b| b == b' ').unwrap_or(3);
    if ext_start > 0 {
        name.push('.');
        for b in &sfn[8..8 + ext_start] {
            name.push((*b as char).to_ascii_lowercase());
        }
    }
    name
}

pub fn decode_volume_label(sfn: &[u8; MAX_SFN_LEN]) -> String {
    let end = sfn.iter().rposition(|&b| b != b' ').map(|p| p + 1).unwrap_or(0);
    core::str::from_utf8(&sfn[..end]).unwrap_or("").trim_end_matches('\0').to_string()
}

pub fn make_sfn_bytes(stem: &str, ext: &str) -> [u8; MAX_SFN_LEN] {
    let mut sfn = [b' '; MAX_SFN_LEN];
    for (i, &b) in stem.as_bytes().iter().enumerate() {
        if i >= 8 { break; }
        sfn[i] = b.to_ascii_uppercase();
    }
    for (i, &b) in ext.as_bytes().iter().enumerate() {
        if i >= 3 { break; }
        sfn[8 + i] = b.to_ascii_uppercase();
    }
    sfn
}

pub fn sfn_from_name(name: &str, existing_sfns: &HashSet<[u8; MAX_SFN_LEN]>) -> Option<[u8; MAX_SFN_LEN]> {
    if name.is_empty() { return None; }
    let (stem, ext) = if let Some(dot) = name.rfind('.') {
        if dot == 0 { ("", &name[1..]) } else { (&name[..dot], &name[dot + 1..]) }
    } else {
        (name, "")
    };

    let base = make_sfn_bytes(stem, ext);
    if !existing_sfns.contains(&base) {
        return Some(base);
    }

    let mut counter = 1u32;
    let mut suffix_buf = [0u8; 7];
    suffix_buf[0] = b'~';
    loop {
        let suffix_len = {
            let mut n = counter;
            let mut p = 6;
            while n > 0 {
                p -= 1;
                suffix_buf[p] = b'0' + (n % 10) as u8;
                n /= 10;
            }
            6 - p
        };
        let suffix_bytes = &suffix_buf[..suffix_len + 1];
        let stem_avail = 8 - suffix_bytes.len();
        let stem_trunc = &stem.as_bytes()[..stem.len().min(stem_avail)];
        let mut sfn = [b' '; MAX_SFN_LEN];
        for (i, &b) in stem_trunc.iter().enumerate() {
            sfn[i] = b.to_ascii_uppercase();
        }
        for (j, &b) in suffix_bytes.iter().enumerate() {
            sfn[stem_avail + j] = b;
        }
        for (i, &b) in ext.as_bytes().iter().enumerate() {
            if i >= 3 { break; }
            sfn[8 + i] = b.to_ascii_uppercase();
        }
        if !existing_sfns.contains(&sfn) {
            return Some(sfn);
        }
        counter += 1;
        if counter > 99999 { return None; }
    }
}

pub fn vfat_checksum(sfn: &[u8; MAX_SFN_LEN]) -> u8 {
    let mut sum: u8 = 0;
    for i in 0..MAX_SFN_LEN {
        sum = ((sum >> 1) | (sum << 7)).wrapping_add(sfn[i]);
    }
    sum
}

pub fn needs_vfat(name: &str) -> bool {
    if name == "." || name == ".." { return false; }
    let dot = name.rfind('.');
    let base_len = dot.unwrap_or(name.len());
    let ext_len = if let Some(d) = dot { name.len() - d - 1 } else { 0 };
    base_len > 8 || ext_len > 3 || name.bytes().any(|b| b > 127 || b == b' ')
}

pub fn decode_vfat_name(entries: &[[u8; DIR_ENTRY_SIZE]]) -> String {
    let mut utf16_buf: Vec<u16> = Vec::new();
    for entry in entries.iter().rev() {
        if entry[0] == DIR_DELETED || entry[0] & 0x1F == 0 { continue; }
        for j in 0..13 {
            let c = get_vfat_char(entry, j);
            if c == 0 || c == 0xFFFF { break; }
            utf16_buf.push(c);
        }
    }
    String::from_utf16_lossy(&utf16_buf)
}

pub fn get_vfat_char(entry: &[u8; DIR_ENTRY_SIZE], index: usize) -> u16 {
    match index {
        0..=4   => u16::from_le_bytes([entry[1 + index * 2], entry[2 + index * 2]]),
        5..=10  => u16::from_le_bytes([entry[14 + (index - 5) * 2], entry[15 + (index - 5) * 2]]),
        11..=12 => u16::from_le_bytes([entry[28 + (index - 11) * 2], entry[29 + (index - 11) * 2]]),
        _ => 0,
    }
}

pub fn set_vfat_char(entry: &mut [u8; DIR_ENTRY_SIZE], index: usize, c: u16) {
    let bytes = c.to_le_bytes();
    match index {
        0..=4   => { entry[1 + index * 2] = bytes[0]; entry[2 + index * 2] = bytes[1]; }
        5..=10  => { entry[14 + (index - 5) * 2] = bytes[0]; entry[15 + (index - 5) * 2] = bytes[1]; }
        11..=12 => { entry[28 + (index - 11) * 2] = bytes[0]; entry[29 + (index - 11) * 2] = bytes[1]; }
        _ => {}
    }
}

pub fn encode_vfat_entries(name: &str, checksum: u8) -> Vec<[u8; DIR_ENTRY_SIZE]> {
    let u16_chars: Vec<u16> = name.encode_utf16().collect();
    let needed = (u16_chars.len() + 12) / 13;
    let mut entries = Vec::with_capacity(needed);
    for i in 0..needed {
        let mut entry = [0u8; DIR_ENTRY_SIZE];
        let start = i * 13;
        let count = (u16_chars.len() - start).min(13);
        let ord = (needed - i) as u8;
        entry[0] = if i == 0 { ord | 0x40 } else { ord };
        entry[11] = ATTR_LONG_NAME;
        entry[12] = 0;
        entry[13] = checksum;
        for j in 0..count { set_vfat_char(&mut entry, j, u16_chars[start + j]); }
        for j in count..13 { set_vfat_char(&mut entry, j, 0xFFFF); }
        entries.push(entry);
    }
    entries.reverse();
    entries
}

pub fn first_clus_from_entry(entry: &[u8; DIR_ENTRY_SIZE]) -> u32 {
    let hi = u16::from_le_bytes([entry[0x14], entry[0x15]]);
    let lo = u16::from_le_bytes([entry[0x1A], entry[0x1B]]);
    (hi as u32) << 16 | lo as u32
}

pub fn set_first_clus_in_entry(entry: &mut [u8; DIR_ENTRY_SIZE], cluster: u32) {
    let lo_bytes = (cluster as u16).to_le_bytes();
    let hi_bytes = ((cluster >> 16) as u16).to_le_bytes();
    entry[0x14] = hi_bytes[0]; entry[0x15] = hi_bytes[1];
    entry[0x1A] = lo_bytes[0]; entry[0x1B] = lo_bytes[1];
}

pub fn file_size_from_entry(entry: &[u8; DIR_ENTRY_SIZE]) -> u32 {
    u32::from_le_bytes([entry[0x1C], entry[0x1D], entry[0x1E], entry[0x1F]])
}

pub fn set_file_size_in_entry(entry: &mut [u8; DIR_ENTRY_SIZE], size: u32) {
    let bytes = size.to_le_bytes();
    entry[0x1C] = bytes[0]; entry[0x1D] = bytes[1];
    entry[0x1E] = bytes[2]; entry[0x1F] = bytes[3];
}