use hashbrown::{HashMap, HashSet};
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;

use super::bpb::{Bpb, SECTOR_SIZE};
use super::io::{read_sectors, write_sectors};

const MAX_FAT_CACHE_ENTRIES: usize = 4096;

pub(crate) struct FatCache {
    sectors: HashMap<u64, [u8; SECTOR_SIZE]>,
    dirty: HashSet<u64>,
    /// Eviction clock: circular buffer of LBAs in insertion order.
    /// Clock hand advances on each eviction, sweeping until finding a non-dirty sector.
    clock: Vec<u64>,
    clock_hand: usize,
}

impl FatCache {
    pub fn new() -> Self {
        FatCache {
            sectors: HashMap::new(),
            dirty: HashSet::new(),
            clock: Vec::with_capacity(MAX_FAT_CACHE_ENTRIES),
            clock_hand: 0,
        }
    }

    pub fn get_or_read(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&[u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict();
            self.sectors.insert(lba, buf);
            self.clock.push(lba);
        }
        match self.sectors.get(&lba) {
            Some(sector) => Ok(sector),
            None => Err(VfsError::IOError),
        }
    }

    pub fn get_or_read_mut(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&mut [u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict();
            self.sectors.insert(lba, buf);
            self.clock.push(lba);
        }
        self.dirty.insert(lba);
        match self.sectors.get_mut(&lba) {
            Some(sector) => Ok(sector),
            None => Err(VfsError::IOError),
        }
    }

    pub fn flush(&mut self, device: &dyn BlockDevice, bpb: &Bpb) -> Result<(), VfsError> {
        let is_mirrored = bpb.ext_flags & 0x80 == 0;
        for &lba in self.dirty.iter() {
            let data = match self.sectors.get(&lba) {
                Some(sector) => sector,
                None => return Err(VfsError::IOError),
            };
            write_sectors(device, lba, 1, data)?;
            if is_mirrored {
                for fat_num in 1..bpb.num_fats {
                    let local_idx = (lba - bpb.fat_sector_lba(0, 0)) % bpb.fat_sz32 as u64;
                    write_sectors(device, bpb.fat_sector_lba(fat_num, local_idx as u32), 1, data)?;
                }
            }
        }
        self.dirty.clear();
        Ok(())
    }

    fn maybe_evict(&mut self) {
        if self.sectors.len() < MAX_FAT_CACHE_ENTRIES {
            return;
        }
        let target = MAX_FAT_CACHE_ENTRIES - MAX_FAT_CACHE_ENTRIES / 4;
        while self.sectors.len() > target {
            if self.clock.is_empty() { break; }
            if self.clock_hand >= self.clock.len() {
                self.clock_hand = 0;
            }
            let lba = self.clock[self.clock_hand];
            if !self.dirty.contains(&lba) {
                self.sectors.remove(&lba);
                self.clock.swap_remove(self.clock_hand);
            } else {
                self.clock_hand += 1;
            }
        }
    }
}