use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::BlockDevice;

use super::{MAX_EBR_CHAIN, PartitionInfo, read_sector};

#[repr(C, packed)]
struct MbrEntry {
    status: u8,
    chs_first: [u8; 3],
    partition_type: u8,
    chs_last: [u8; 3],
    lba_start: u32,
    sector_count: u32,
}

pub fn parse(
    device: Arc<dyn BlockDevice>,
    mbr_sector: &[u8; 512],
) -> Result<Vec<PartitionInfo>, &'static str> {
    let dev_sectors = device.sector_count();
    let mut partitions: Vec<PartitionInfo> = Vec::new();
    let mut ext_base_lba: Option<u64> = None;
    let mut ext_end_excl: Option<u64> = None;

    for i in 0..4 {
        let offset = 0x1BE + i * 16;
        let entry = unsafe {
            core::ptr::read_unaligned(mbr_sector.as_ptr().add(offset) as *const MbrEntry)
        };
        let typ = entry.partition_type;
        if typ == 0 {
            continue;
        }
        // Protective-MBR stubs (any slot, e.g. hybrid layouts) are GPT
        // placeholders, not real partitions.
        if typ == 0xEE {
            continue;
        }
        let start_lba = u32::from_le(entry.lba_start) as u64;
        let sector_count = u32::from_le(entry.sector_count) as u64;
        if sector_count == 0 {
            // Degenerate entry: end_lba would underflow to u64::MAX.
            continue;
        }
        let end_excl = start_lba
            .checked_add(sector_count)
            .ok_or("partition range overflow")?;
        if end_excl > dev_sectors {
            return Err("partition extends beyond device");
        }
        let is_ext = typ == 0x05 || typ == 0x0F;

        partitions.push(PartitionInfo {
            number: (i + 1) as u32,
            start_lba,
            end_lba: end_excl - 1,
            size_sectors: sector_count,
            partition_type: Some(typ),
            guid_type: None,
            guid_unique: None,
            name: None,
            is_extended: is_ext,
        });

        if is_ext {
            ext_base_lba = Some(start_lba);
            ext_end_excl = Some(end_excl);
        }
    }

    if let Some(ext_base) = ext_base_lba {
        let ext_limit = ext_end_excl.unwrap_or(dev_sectors);
        let mut next_ebr_lba = ext_base;
        let mut logical_num: u32 = 5;
        let mut chain_count: u32 = 0;

        while chain_count < MAX_EBR_CHAIN {
            let mut ebr = [0u8; 512];
            read_sector(&*device, next_ebr_lba, &mut ebr)?;

            if ebr[510] != 0x55 || ebr[511] != 0xAA {
                break;
            }

            let entry0 =
                unsafe { core::ptr::read_unaligned(ebr.as_ptr().add(0x1BE) as *const MbrEntry) };
            let typ0 = entry0.partition_type;

            if typ0 != 0 {
                let rel_start = u32::from_le(entry0.lba_start) as u64;
                let sector_count = u32::from_le(entry0.sector_count) as u64;
                if sector_count > 0 {
                    let abs_lba = next_ebr_lba
                        .checked_add(rel_start)
                        .ok_or("EBR chain overflow")?;
                    let end_excl = abs_lba
                        .checked_add(sector_count)
                        .ok_or("partition range overflow")?;
                    if end_excl > dev_sectors {
                        return Err("logical partition extends beyond device");
                    }
                    if end_excl > ext_limit {
                        return Err("logical partition outside extended extent");
                    }
                    partitions.push(PartitionInfo {
                        number: logical_num,
                        start_lba: abs_lba,
                        end_lba: end_excl - 1,
                        size_sectors: sector_count,
                        partition_type: Some(typ0),
                        guid_type: None,
                        guid_unique: None,
                        name: None,
                        is_extended: false,
                    });
                    logical_num += 1;
                }
            }

            let entry1 =
                unsafe { core::ptr::read_unaligned(ebr.as_ptr().add(0x1CE) as *const MbrEntry) };
            let typ1 = entry1.partition_type;
            let next_rel = u32::from_le(entry1.lba_start) as u64;

            if typ1 == 0 || next_rel == 0 {
                break;
            }

            next_ebr_lba = ext_base.checked_add(next_rel).ok_or("EBR chain overflow")?;
            chain_count += 1;
        }
    }

    Ok(partitions)
}
