use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;

use super::io::read_sectors;

pub(crate) const BYTES_PER_SECTOR: u64 = 512;

/// The NTFS boot sector (sector 0 of the volume).  Only the fields the
/// read-only driver needs are decoded; anything exotic (4K sectors, absurd
/// cluster counts) is rejected with a plain error rather than guessed at.
#[derive(Clone)]
pub(crate) struct BootSector {
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub sectors_per_cluster: u8,
    pub bytes_per_sector: u64,
    /// Size of one MFT record (power of two, multiple of the sector size).
    pub record_size: u64,
    /// Size of one index buffer (INDX block).
    pub index_size: u64,
    /// Upper bound on valid MFT record numbers.
    pub max_records: u64,
}

/// Decode the signed "clusters per record/buffer" field: a positive value is
/// clusters, a negative value means the unit is 1 << -value bytes.
fn unit_size(value: i8, cluster_size: u64) -> Result<u64, VfsError> {
    if value > 0 {
        let bytes = cluster_size
            .checked_mul(value as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(bytes)
    } else if value < 0 {
        let shift = (-(value as i64)) as u32;
        if shift > 16 {
            return Err(VfsError::InvalidInput);
        }
        Ok(1u64 << shift)
    } else {
        Err(VfsError::InvalidInput)
    }
}

impl BootSector {
    pub fn parse(device: &dyn BlockDevice) -> Result<BootSector, VfsError> {
        let mut sector = [0u8; BYTES_PER_SECTOR as usize];
        read_sectors(device, 0, 1, &mut sector)?;

        if sector[0x03..0x0B] != *b"NTFS    " {
            return Err(VfsError::InvalidDevice);
        }

        let bytes_per_sector = u16::from_le_bytes([sector[0x0B], sector[0x0C]]) as u64;
        if bytes_per_sector != BYTES_PER_SECTOR {
            return Err(VfsError::InvalidInput);
        }

        let sectors_per_cluster = sector[0x0D];
        if sectors_per_cluster == 0
            || !sectors_per_cluster.is_power_of_two()
            || sectors_per_cluster > 128
        {
            return Err(VfsError::InvalidInput);
        }

        let total_sectors = u64::from_le_bytes(
            sector[0x28..0x30]
                .try_into()
                .map_err(|_| VfsError::IOError)?,
        );
        if total_sectors == 0 || total_sectors > device.sector_count() {
            return Err(VfsError::InvalidInput);
        }

        let mft_lcn = u64::from_le_bytes(
            sector[0x30..0x38]
                .try_into()
                .map_err(|_| VfsError::IOError)?,
        );
        if mft_lcn
            .checked_mul(sectors_per_cluster as u64)
            .map_or(true, |lba| lba >= total_sectors)
        {
            return Err(VfsError::InvalidInput);
        }

        let cluster_size = bytes_per_sector * sectors_per_cluster as u64;
        let record_size = unit_size(sector[0x40] as i8, cluster_size)?;
        if record_size < BYTES_PER_SECTOR
            || record_size > 65536
            || !record_size.is_power_of_two()
            || record_size % bytes_per_sector != 0
        {
            return Err(VfsError::InvalidInput);
        }

        let index_size = unit_size(sector[0x44] as i8, cluster_size)?;
        if index_size < BYTES_PER_SECTOR
            || index_size > 65536
            || !index_size.is_power_of_two()
            || index_size % bytes_per_sector != 0
        {
            return Err(VfsError::InvalidInput);
        }

        // Sanity cap: the MFT cannot hold more records than the volume has
        // record-sized units (even counting the MFT's own growth areas).
        let max_records = total_sectors
            .saturating_mul(bytes_per_sector)
            .checked_div(record_size)
            .filter(|&n| n > 0)
            .ok_or(VfsError::InvalidInput)?;

        Ok(BootSector {
            total_sectors,
            mft_lcn,
            sectors_per_cluster,
            bytes_per_sector,
            record_size,
            index_size,
            max_records,
        })
    }

    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector * self.sectors_per_cluster as u64
    }

    pub fn cluster_to_lba(&self, cluster: u64) -> u64 {
        cluster * self.sectors_per_cluster as u64
    }
}
