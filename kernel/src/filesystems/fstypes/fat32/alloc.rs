use core::sync::atomic::Ordering;

use alloc::vec::Vec;

use crate::filesystems::vfs::error::VfsError;

use super::bpb::SECTOR_SIZE;
use super::fat::{EOC_MARKER, FREE_CLUSTER, FSINFO_LEAD_SIG, FSINFO_STRUCT_SIG, FSINFO_TRAIL_SIG};
use super::io::read_sectors;

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

use super::mount::Fat32SuperBlock;

/// In-RAM allocation bitmap over clusters [2, 2+total_clus).
/// Bit set = cluster allocated.  Replaces per-allocation linear FAT scans
/// that read up to millions of sectors through the cache.
/// Cap keeps worst-case RAM bounded (~2 MiB at 16M clusters); larger volumes
/// transparently fall back to the FAT-scan allocator.
pub(crate) struct AllocBitmap {
    words: Vec<u64>,
}

const BITMAP_MAX_CLUSTERS: usize = 16 * 1024 * 1024;

impl AllocBitmap {
    fn new(total_clus: usize) -> Self {
        AllocBitmap {
            words: alloc::vec![0u64; total_clus.div_ceil(64)],
        }
    }

    #[inline]
    fn bit_index(clus: u32, total: usize) -> Option<usize> {
        let i = clus.checked_sub(2)? as usize;
        if i < total {
            Some(i)
        } else {
            None
        }
    }
}

impl Fat32SuperBlock {
    pub fn alloc_cluster(&self) -> Result<u32, VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] alloc_cluster\n"
        ));
        let mut hint = self.next_alloc_hint.lock();
        let n = self.bpb.total_clus;

        // Fast path: bitmap scan (memory only, no FAT reads).
        // Hold bitmap only to reserve a free bit; drop it before doing FAT I/O
        // so we don't hold `alloc_bitmap -> fat_cache` while `free_chain`
        // holds `fat_cache -> alloc_bitmap`.
        {
            let mut candidate: Option<(u32, usize, u64)> = None;
            {
                let mut bmp = self.alloc_bitmap.lock();
                if let Some(b) = bmp.as_mut() {
                    let total = n as usize;
                    if total > 0 {
                        let start = (*hint as usize).saturating_sub(2) % total;
                        for off in 0..total {
                            let i = (start + off) % total;
                            let w = i / 64;
                            let bit = 1u64 << (i % 64);
                            if b.words[w] & bit != 0 {
                                continue;
                            }
                            let clus = 2 + i as u32;
                            b.words[w] |= bit;
                            candidate = Some((clus, w, bit));
                            break;
                        }
                        if candidate.is_none() {
                            return Err(VfsError::NoSpace);
                        }
                    }
                }
            }
            if let Some((clus, w, bit)) = candidate {
                match self.write_fat_entry(clus, EOC_MARKER) {
                    Ok(()) => {
                        *hint = clus + 1;
                        self.free_clus_count.fetch_sub(1, Ordering::Relaxed);
                        return Ok(clus);
                    }
                    Err(e) => {
                        // Roll back the reservation so the bitmap stays in sync.
                        if let Some(b) = self.alloc_bitmap.lock().as_mut() {
                            b.words[w] &= !bit;
                        }
                        return Err(e);
                    }
                }
            }
        }

        // Fallback: rotating linear scan over the FAT itself.
        for i in 0..n {
            let clus = 2 + ((*hint - 2 + i) % n);
            if self.read_fat_entry(clus)? == FREE_CLUSTER {
                self.write_fat_entry(clus, EOC_MARKER)?;
                *hint = clus + 1;
                self.free_clus_count.fetch_sub(1, Ordering::Relaxed);
                return Ok(clus);
            }
        }
        Err(VfsError::NoSpace)
    }

    /// Keep the allocation bitmap in sync with a freed cluster.
    fn bitmap_mark_free(&self, cluster: u32) {
        let mut bmp = self.alloc_bitmap.lock();
        if let Some(b) = bmp.as_mut() {
            if let Some(i) = AllocBitmap::bit_index(cluster, self.bpb.total_clus as usize) {
                b.words[i / 64] &= !(1u64 << (i % 64));
            }
        }
    }

    pub fn free_chain(&self, mut cluster: u32) -> Result<(), VfsError> {
        let mut _iters = 0u32;
        while cluster >= 2 && cluster < EOC_MARKER {
            let next = self.read_fat_entry(cluster)?;
            self.write_fat_entry(cluster, FREE_CLUSTER)?;
            self.bitmap_mark_free(cluster);
            self.free_clus_count.fetch_add(1, Ordering::Relaxed);
            cluster = next;
            _iters += 1;
            if _iters > self.bpb.total_clus + 2 {
                return Err(VfsError::IOError);
            }
        }
        Ok(())
    }

    pub fn scan_free_clusters(&self) -> Result<u32, VfsError> {
        let fat_idx = self.bpb.active_fat_idx();
        let first_lba = self.bpb.fat_sector_lba(fat_idx, 0);
        let nsec = self.bpb.fat_sz32;
        let total_clus = self.bpb.total_clus as u64;
        let mut count = 0u32;
        let mut buf = [0u8; SECTOR_SIZE];
        let first_valid = 2u64;
        let last_valid = first_valid + total_clus - 1;

        // Build the allocation bitmap during this same pass -- it costs one
        // extra Vec at mount time and turns every later allocation O(bits)
        // instead of O(FAT sectors through cache).
        let mut bitmap = if total_clus <= BITMAP_MAX_CLUSTERS as u64 {
            Some(AllocBitmap::new(total_clus as usize))
        } else {
            log::warn!("FAT32: {} clusters exceeds bitmap cap; using FAT-scan allocator", total_clus);
            None
        };

        for sec in 0..nsec as u64 {
            read_sectors(&*self.device, first_lba + sec, 1, &mut buf)?;
            let entry_base = sec * 128;
            for i in 0..128 {
                let entry_idx = entry_base + i;
                if entry_idx < first_valid || entry_idx > last_valid {
                    continue;
                }
                let off = (i * 4) as usize;
                let val = u32::from_le_bytes(
                    buf.get(off..off + 4)
                        .ok_or(VfsError::IOError)?
                        .try_into()
                        .map_err(|_| VfsError::IOError)?,
                );
                if val & 0x0FFFFFFF == FREE_CLUSTER {
                    count += 1;
                } else if let Some(b) = bitmap.as_mut() {
                    let ci = entry_idx as usize - 2;
                    b.words[ci / 64] |= 1u64 << (ci % 64);
                }
            }
        }

        *self.alloc_bitmap.lock() = bitmap;
        Ok(count)
    }

    pub fn write_fsinfo(&self) -> Result<(), VfsError> {
        if !self.bpb.fsinfo_is_valid() {
            return Ok(());
        }
        let sec = self.bpb.fsinfo_sec as u64;
        let mut buf = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, sec, 1, &mut buf)?;

        buf[0..4].copy_from_slice(&FSINFO_LEAD_SIG.to_le_bytes());
        buf[484..488].copy_from_slice(&FSINFO_STRUCT_SIG.to_le_bytes());
        buf[488..492].copy_from_slice(&self.free_clus_count.load(Ordering::Relaxed).to_le_bytes());
        buf[492..496].copy_from_slice(&(*self.next_alloc_hint.lock()).to_le_bytes());
        buf[508..512].copy_from_slice(&FSINFO_TRAIL_SIG.to_le_bytes());
        super::io::write_sectors(&*self.device, sec, 1, &buf)
    }
}
