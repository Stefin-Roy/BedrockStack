use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;

use super::io::read_sectors;

pub(crate) const SECTOR_SIZE: usize = 512;
pub(crate) const DIR_ENTRY_SIZE: usize = 32;
pub(crate) const MAX_SFN_LEN: usize = 11;

#[derive(Clone)]
pub(crate) struct Bpb {
    pub bytes_per_sec: u16,
    pub sec_per_clus: u8,
    pub rsvd_sec_cnt: u16,
    pub num_fats: u8,
    pub fat_sz32: u32,
    pub root_clus: u32,
    pub fsinfo_sec: u16,
    pub byts_per_clus: u32,
    pub first_data_sec: u64,
    pub total_clus: u32,
    pub active_fat: u8,
}

pub(super) fn parse_bpb(device: &dyn BlockDevice) -> Result<Bpb, VfsError> {
    let mut sector = [0u8; SECTOR_SIZE];
    read_sectors(device, 0, 1, &mut sector)?;

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(VfsError::InvalidDevice);
    }

    let bytes_per_sec = u16::from_le_bytes([sector[0x0B], sector[0x0C]]);
    let sec_per_clus = sector[0x0D];
    let rsvd_sec_cnt = u16::from_le_bytes([sector[0x0E], sector[0x0F]]);
    let num_fats = sector[0x10];
    let root_ent_cnt = u16::from_le_bytes([sector[0x11], sector[0x12]]);
    let fat_sz16 = u16::from_le_bytes([sector[0x16], sector[0x17]]);
    let fat_sz32 = u32::from_le_bytes([sector[0x24], sector[0x25], sector[0x26], sector[0x27]]);
    let root_clus = u32::from_le_bytes([sector[0x2C], sector[0x2D], sector[0x2E], sector[0x2F]]);
    let fsinfo_sec = u16::from_le_bytes([sector[0x30], sector[0x31]]);

    if root_ent_cnt != 0 || fat_sz16 != 0 || fat_sz32 == 0 {
        return Err(VfsError::InvalidDevice);
    }

    if bytes_per_sec != SECTOR_SIZE as u16 {
        return Err(VfsError::InvalidInput);
    }
    if sec_per_clus == 0 || !sec_per_clus.is_power_of_two() || sec_per_clus > 128 {
        return Err(VfsError::InvalidInput);
    }
    if rsvd_sec_cnt == 0 {
        return Err(VfsError::InvalidInput);
    }
    if num_fats == 0 {
        return Err(VfsError::InvalidInput);
    }
    if root_clus < 2 {
        return Err(VfsError::InvalidInput);
    }

    let first_data_sec = rsvd_sec_cnt as u64 + (num_fats as u64) * fat_sz32 as u64;

    let total_sectors = {
        let sz16 = u16::from_le_bytes([sector[0x13], sector[0x14]]);
        if sz16 != 0 { sz16 as u64 } else {
            u32::from_le_bytes([sector[0x20], sector[0x21], sector[0x22], sector[0x23]]) as u64
        }
    };

    if total_sectors <= first_data_sec {
        return Err(VfsError::InvalidInput);
    }
    if total_sectors > device.sector_count() {
        return Err(VfsError::InvalidInput);
    }

    let total_data_sectors = total_sectors - first_data_sec;
    let total_clus = (total_data_sectors / sec_per_clus as u64) as u32;
    let byts_per_clus = (bytes_per_sec as u32) * (sec_per_clus as u32);

    let active_fat = sector[0x42];

    Ok(Bpb {
        bytes_per_sec, sec_per_clus, rsvd_sec_cnt, num_fats,
        fat_sz32, root_clus, fsinfo_sec,
        byts_per_clus, first_data_sec, total_clus, active_fat,
    })
}

impl Bpb {
    pub fn cluster_to_lba(&self, cluster: u32) -> u64 {
        let offset = (cluster.saturating_sub(2)) as u64;
        self.first_data_sec + offset * (self.sec_per_clus as u64)
    }

    pub fn active_fat_idx(&self) -> u8 {
        if self.active_fat & 0x80 != 0 { self.active_fat & 0x0F } else { 0 }
    }

    pub fn fat_sector_lba(&self, fat_num: u8, sector_idx: u32) -> u64 {
        self.rsvd_sec_cnt as u64
            + (fat_num as u64) * self.fat_sz32 as u64
            + sector_idx as u64
    }

    pub fn fat_entry_position(&self, cluster: u32) -> (u32, u32) {
        let byte_off = cluster as u32 * 4;
        if self.bytes_per_sec == 0 {
            crate::drivers::serial::dump_puts("[BPB] CORRUPT: bytes_per_sec=0 in fat_entry_position!\n");
            return (0, 0);
        }
        let sector_idx = byte_off / self.bytes_per_sec as u32;
        let offset = byte_off % self.bytes_per_sec as u32;
        (sector_idx, offset)
    }

    pub fn fsinfo_is_valid(&self) -> bool {
        self.fsinfo_sec != 0 && self.fsinfo_sec != 0xFFFF
    }
}