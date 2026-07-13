use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::BlockDevice;

use super::{crc32, read_sector, read_sectors, PartitionInfo, SECTOR_SIZE};

#[repr(C, packed)]
struct GptHeader {
    signature: [u8; 8],
    revision: u32,
    header_size: u32,
    header_crc32: u32,
    _reserved: [u8; 4],
    my_lba: u64,
    alternate_lba: u64,
    first_usable_lba: u64,
    last_usable_lba: u64,
    disk_guid: [u8; 16],
    partition_entry_lba: u64,
    num_partition_entries: u32,
    partition_entry_size: u32,
    partition_entries_crc32: u32,
}

#[repr(C, packed)]
struct GptEntry {
    partition_type_guid: [u8; 16],
    unique_guid: [u8; 16],
    starting_lba: u64,
    ending_lba: u64,
    attributes: u64,
    name: [u16; 36],
}

pub fn parse(device: Arc<dyn BlockDevice>) -> Result<Vec<PartitionInfo>, &'static str> {
    let mut buf = [0u8; 512];
    read_sector(&*device, 1, &mut buf)?;

    let hdr = unsafe { &*(buf.as_ptr() as *const GptHeader) };

    if &hdr.signature != b"EFI PART" {
        return Err("GPT signature not found");
    }

    let header_size = u32::from_le(hdr.header_size) as usize;
    if header_size < 92 || header_size > 512 {
        return Err("GPT header size out of range");
    }

    let stored_crc = u32::from_le(hdr.header_crc32);
    let mut crc_buf = [0u8; 512];
    crc_buf[..header_size].copy_from_slice(&buf[..header_size]);
    crc_buf[16..20].copy_from_slice(&[0, 0, 0, 0]);
    if crc32(&crc_buf[..header_size]) != stored_crc {
        return Err("GPT header CRC mismatch");
    }

    let entry_lba = u64::from_le(hdr.partition_entry_lba);
    let num_entries = u32::from_le(hdr.num_partition_entries);
    let entry_size = u32::from_le(hdr.partition_entry_size) as usize;
    if entry_size < 128 {
        return Err("GPT entry size too small");
    }

    let total_bytes = num_entries as usize * entry_size;
    let sector_count = (total_bytes + SECTOR_SIZE - 1) / SECTOR_SIZE;
    let mut entries_buf = alloc::vec![0u8; sector_count * SECTOR_SIZE];
    read_sectors(&*device, entry_lba, sector_count as u32, &mut entries_buf)?;

    let mut partitions: Vec<PartitionInfo> = Vec::new();

    for i in 0..num_entries as usize {
        let offset = i * entry_size;
        if offset + 128 > entries_buf.len() {
            break;
        }
        let entry = unsafe { &*(entries_buf.as_ptr().add(offset) as *const GptEntry) };

        if entry.partition_type_guid == [0u8; 16] {
            continue;
        }

        let start_lba = u64::from_le(entry.starting_lba);
        let end_lba = u64::from_le(entry.ending_lba);
        let size_sectors = end_lba - start_lba + 1;

        let name_units = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(entry.name)) };
        let name_str = decode_utf16_le(&name_units);

        partitions.push(PartitionInfo {
            number: (i + 1) as u32,
            start_lba,
            end_lba,
            size_sectors,
            partition_type: 0xEE,
            guid_type: Some(entry.partition_type_guid),
            guid_unique: Some(entry.unique_guid),
            name: if name_str.is_empty() { None } else { Some(name_str) },
            is_extended: false,
        });
    }

    Ok(partitions)
}

fn decode_utf16_le(units: &[u16; 36]) -> String {
    let mut result = String::new();
    let mut i = 0;
    while i < 36 {
        let u = u16::from_le(units[i]);
        if u == 0 {
            break;
        }
        match u {
            0xD800..=0xDBFF => {
                if i + 1 < 36 {
                    let u2 = u16::from_le(units[i + 1]);
                    if (0xDC00..=0xDFFF).contains(&u2) {
                        let cp = ((u as u32 - 0xD800) << 10) | (u2 as u32 - 0xDC00) | 0x1_0000;
                        result.push(core::char::from_u32(cp).unwrap_or(
                            core::char::REPLACEMENT_CHARACTER,
                        ));
                        i += 2;
                        continue;
                    }
                }
                result.push(core::char::REPLACEMENT_CHARACTER);
            }
            0xDC00..=0xDFFF => {
                result.push(core::char::REPLACEMENT_CHARACTER);
            }
            _ => {
                result.push(core::char::from_u32(u as u32).unwrap_or(
                    core::char::REPLACEMENT_CHARACTER,
                ));
            }
        }
        i += 1;
    }
    result
}
