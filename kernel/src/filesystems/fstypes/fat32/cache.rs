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
    access_gen: HashMap<u64, u64>,
    gen_counter: u64,
}

impl FatCache {
    pub fn new() -> Self {
        FatCache {
            sectors: HashMap::new(),
            dirty: HashSet::new(),
            access_gen: HashMap::new(),
            gen_counter: 0,
        }
    }

    pub fn get_or_read(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&[u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict();
            self.sectors.insert(lba, buf);
        }
        self.touch(lba);
        Ok(self.sectors.get(&lba).unwrap())
    }

    pub fn get_or_read_mut(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&mut [u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict();
            self.sectors.insert(lba, buf);
        }
        self.dirty.insert(lba);
        self.touch(lba);
        Ok(self.sectors.get_mut(&lba).unwrap())
    }

    pub fn flush(&mut self, device: &dyn BlockDevice, bpb: &Bpb) -> Result<(), VfsError> {
        let is_mirrored = bpb.active_fat & 0x80 == 0;
        for &lba in self.dirty.iter() {
            let data = self.sectors.get(&lba).unwrap();
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

    fn touch(&mut self, lba: u64) {
        let gen_val = self.gen_counter;
        self.gen_counter = gen_val.wrapping_add(1);
        self.access_gen.insert(lba, gen_val);
    }

    fn maybe_evict(&mut self) {
        if self.sectors.len() < MAX_FAT_CACHE_ENTRIES {
            return;
        }
        let target = MAX_FAT_CACHE_ENTRIES - MAX_FAT_CACHE_ENTRIES / 4;
        let mut evictable: Vec<(u64, u64)> = self.sectors.keys()
            .filter_map(|lba| {
                if self.dirty.contains(lba) { return None; }
                Some((*lba, self.access_gen.get(lba).copied().unwrap_or(0)))
            })
            .collect();
        evictable.sort_by_key(|(_, g)| *g);
        let n_evict = self.sectors.len().saturating_sub(target);
        for (lba, _) in evictable.iter().take(n_evict) {
            self.sectors.remove(lba);
            self.access_gen.remove(lba);
        }
    }
}