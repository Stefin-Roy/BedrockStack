use crate::filesystems::vfs::error::VfsError;

pub(super) const EOC_MARKER: u32 = 0x0FFFFFF8;
pub(super) const FREE_CLUSTER: u32 = 0x00000000;

pub(super) const FSINFO_LEAD_SIG: u32 = 0x41615252;
pub(super) const FSINFO_STRUCT_SIG: u32 = 0x61417272;
pub(super) const FSINFO_TRAIL_SIG: u32 = 0xAA550000;

use super::mount::Fat32SuperBlock;

impl Fat32SuperBlock {
    pub fn read_fat_entry(&self, cluster: u32) -> Result<u32, VfsError> {
        let (sector_idx, offset) = self.bpb.fat_entry_position(cluster);
        let fat_idx = self.bpb.active_fat_idx();
        let lba = self.bpb.fat_sector_lba(fat_idx, sector_idx);
        let mut cache = self.fat_cache.lock();
        let sector = cache.get_or_read(&*self.device, lba)?;
        let val = u32::from_le_bytes([
            sector[offset as usize], sector[offset as usize + 1],
            sector[offset as usize + 2], sector[offset as usize + 3],
        ]);
        Ok(val & 0x0FFFFFFF)
    }

    pub fn write_fat_entry(&self, cluster: u32, value: u32) -> Result<(), VfsError> {
        let (sector_idx, offset) = self.bpb.fat_entry_position(cluster);
        let fat_idx = self.bpb.active_fat_idx();
        let lba = self.bpb.fat_sector_lba(fat_idx, sector_idx);
        let mut cache = self.fat_cache.lock();
        let sector = cache.get_or_read_mut(&*self.device, lba)?;
        let bytes = (value & 0x0FFFFFFF).to_le_bytes();
        sector[offset as usize..offset as usize + 4].copy_from_slice(&bytes);
        Ok(())
    }

    pub fn flush_fat_cache(&self) -> Result<(), VfsError> {
        let mut cache = self.fat_cache.lock();
        cache.flush(&*self.device, &self.bpb)
    }
}