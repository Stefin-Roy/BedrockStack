use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

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
    /// FAT layout copied from the BPB so eviction can flush dirty sectors
    /// (including mirror copies) without needing the caller to pass it in.
    fat0_lba: u64,
    fat_sz: u32,
    num_fats: u8,
    mirrored: bool,
}

impl FatCache {
    pub fn new(bpb: &Bpb) -> Self {
        FatCache {
            sectors: HashMap::new(),
            dirty: HashSet::new(),
            clock: Vec::with_capacity(MAX_FAT_CACHE_ENTRIES),
            clock_hand: 0,
            fat0_lba: bpb.fat_sector_lba(0, 0),
            fat_sz: bpb.fat_sz32,
            num_fats: bpb.num_fats,
            mirrored: bpb.ext_flags & 0x80 == 0,
        }
    }

    pub fn get_or_read(
        &mut self,
        device: &dyn BlockDevice,
        lba: u64,
    ) -> Result<&[u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict(device);
            self.sectors.insert(lba, buf);
            self.clock.push(lba);
        }
        self.sectors.get(&lba).ok_or(VfsError::IOError)
    }

    pub fn get_or_read_mut(
        &mut self,
        device: &dyn BlockDevice,
        lba: u64,
    ) -> Result<&mut [u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict(device);
            self.sectors.insert(lba, buf);
            self.clock.push(lba);
        }
        self.dirty.insert(lba);
        self.sectors.get_mut(&lba).ok_or(VfsError::IOError)
    }

    pub fn flush(&mut self, device: &dyn BlockDevice) -> Result<(), VfsError> {
        let is_mirrored = self.mirrored;
        for &lba in self.dirty.iter() {
            let data = self.sectors.get(&lba).ok_or(VfsError::IOError)?;
            write_sectors(device, lba, 1, data)?;
            if is_mirrored {
                for fat_num in 1..self.num_fats {
                    let local_idx = (lba - self.fat0_lba) % self.fat_sz as u64;
                    write_sectors(
                        device,
                        self.fat0_lba + (fat_num as u64) * self.fat_sz as u64 + local_idx,
                        1,
                        data,
                    )?;
                }
            }
        }
        self.dirty.clear();
        Ok(())
    }

    /// Write one dirty sector back (primary + mirrors) and drop it from the
    /// cache.  Best-effort: a failed write leaves the sector cached and
    /// dirty for a later flush.  Returns true if the sector was evicted.
    fn writeback_and_drop(&mut self, device: &dyn BlockDevice, lba: u64) -> bool {
        let data = match self.sectors.get(&lba) {
            Some(d) => *d,
            None => return false,
        };
        if write_sectors(device, lba, 1, &data).is_err() {
            return false;
        }
        if self.mirrored {
            let local_idx = (lba - self.fat0_lba) % self.fat_sz as u64;
            for fat_num in 1..self.num_fats {
                let mirror_lba =
                    self.fat0_lba + (fat_num as u64) * self.fat_sz as u64 + local_idx;
                if write_sectors(device, mirror_lba, 1, &data).is_err() {
                    return false;
                }
            }
        }
        self.dirty.remove(&lba);
        self.sectors.remove(&lba);
        if let Some(pos) = self.clock.iter().position(|&x| x == lba) {
            self.clock.swap_remove(pos);
        }
        true
    }

    /// One clean-eviction pass over the clock.  Returns true once at/below
    /// target.
    fn sweep_clean(&mut self, target: usize) -> bool {
        let mut swept_dirty = 0usize;
        while self.sectors.len() > target && !self.clock.is_empty() {
            if swept_dirty >= self.clock.len() {
                break;
            }
            if self.clock_hand >= self.clock.len() {
                self.clock_hand = 0;
            }
            let lba = self.clock[self.clock_hand];
            if !self.dirty.contains(&lba) {
                self.sectors.remove(&lba);
                self.clock.swap_remove(self.clock_hand);
            } else {
                self.clock_hand += 1;
                swept_dirty += 1;
            }
        }
        self.sectors.len() <= target
    }

    fn maybe_evict(&mut self, device: &dyn BlockDevice) {
        if self.sectors.len() < MAX_FAT_CACHE_ENTRIES {
            return;
        }
        let target = MAX_FAT_CACHE_ENTRIES - MAX_FAT_CACHE_ENTRIES / 4;
        if self.sweep_clean(target) {
            return;
        }
        // Deadlock guard: an all-dirty cache would previously spin forever
        // in here while holding the FatCache mutex.  Flush a batch of dirty
        // sectors synchronously, then retry one clean sweep.
        let need = self.sectors.len().saturating_sub(target);
        let mut batch: Vec<u64> = Vec::new();
        for &lba in self.clock.iter() {
            if batch.len() >= need {
                break;
            }
            if self.dirty.contains(&lba) {
                batch.push(lba);
            }
        }
        for lba in batch {
            self.writeback_and_drop(device, lba);
        }
        self.clock_hand = 0;
        let _ = self.sweep_clean(target);
    }
}
