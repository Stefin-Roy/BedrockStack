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
        let v = self.alloc_clusters(1)?;
        Ok(v[0])
    }

    /// Bulk allocate `count` clusters, returning them in allocation order.
    /// Tries to find a contiguous free run first for better sequential I/O;
    /// falls back to gathering any free clusters. Keeps alloc_bitmap in sync
    /// and writes EOC for each new cluster. Caller must link the chain and
    /// flush.
    pub fn alloc_clusters(&self, count: u32) -> Result<Vec<u32>, VfsError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] alloc_clusters\n"
        ));
        let mut hint = self.next_alloc_hint.lock();
        let n = self.bpb.total_clus;
        if n == 0 {
            return Err(VfsError::NoSpace);
        }

        // Fast path: bitmap. Try to find a contiguous window of `count` free
        // clusters first (best for sequential read/write), otherwise fall back
        // to any free clusters.
        let mut candidates: Vec<(u32, usize, u64)> = Vec::with_capacity(count as usize);
        {
            let mut bmp = self.alloc_bitmap.lock();
            if let Some(b) = bmp.as_mut() {
                let total = n as usize;
                let start = (*hint as usize).saturating_sub(2) % total;
                // Search for contiguous run.
                let mut found_contig = false;
                if (count as usize) <= total {
                    for off in 0..total {
                        let s = (start + off) % total;
                        if s + count as usize > total {
                            // Wrap test needs two checks; simplify by scanning
                            // linearly with wrap via modulo per element.
                            let mut ok = true;
                            for k in 0..count as usize {
                                let idx = (s + k) % total;
                                let w = idx / 64;
                                let bit = 1u64 << (idx % 64);
                                if b.words[w] & bit != 0 {
                                    ok = false;
                                    break;
                                }
                            }
                            if ok {
                                for k in 0..count as usize {
                                    let idx = (s + k) % total;
                                    let w = idx / 64;
                                    let bit = 1u64 << (idx % 64);
                                    b.words[w] |= bit;
                                    candidates.push((2 + idx as u32, w, bit));
                                }
                                found_contig = true;
                                break;
                            }
                        } else {
                            // Fast check without per-element modulo.
                            let mut ok = true;
                            for k in 0..count as usize {
                                let idx = s + k;
                                let w = idx / 64;
                                let bit = 1u64 << (idx % 64);
                                if b.words[w] & bit != 0 {
                                    ok = false;
                                    break;
                                }
                            }
                            if ok {
                                for k in 0..count as usize {
                                    let idx = s + k;
                                    let w = idx / 64;
                                    let bit = 1u64 << (idx % 64);
                                    b.words[w] |= bit;
                                    candidates.push((2 + idx as u32, w, bit));
                                }
                                found_contig = true;
                                break;
                            }
                        }
                    }
                }
                if !found_contig {
                    // Gather any free clusters, not necessarily contiguous.
                    candidates.clear();
                    for off in 0..total {
                        if candidates.len() >= count as usize {
                            break;
                        }
                        let i = (start + off) % total;
                        let w = i / 64;
                        let bit = 1u64 << (i % 64);
                        if b.words[w] & bit != 0 {
                            continue;
                        }
                        let clus = 2 + i as u32;
                        b.words[w] |= bit;
                        candidates.push((clus, w, bit));
                    }
                    if candidates.len() < count as usize {
                        // Roll back partial reservation.
                        for (_, w, bit) in &candidates {
                            b.words[*w] &= !*bit;
                        }
                        return Err(VfsError::NoSpace);
                    }
                } else if candidates.len() != count as usize {
                    // Should not happen; roll back.
                    for (_, w, bit) in &candidates {
                        b.words[*w] &= !*bit;
                    }
                    return Err(VfsError::IOError);
                }
            }
        }
        if !candidates.is_empty() {
            let mut out = Vec::with_capacity(count as usize);
            for (clus, _w, _bit) in &candidates {
                match self.write_fat_entry(*clus, EOC_MARKER) {
                    Ok(()) => {
                        out.push(*clus);
                    }
                    Err(e) => {
                        // Roll back bitmap and any already-written EOCs.
                        let mut bmp = self.alloc_bitmap.lock();
                        if let Some(b) = bmp.as_mut() {
                            for (_, ww, bb) in &candidates {
                                b.words[*ww] &= !*bb;
                            }
                        }
                        for c in out {
                            let _ = self.write_fat_entry(c, FREE_CLUSTER);
                        }
                        return Err(e);
                    }
                }
            }
            // Advance hint past the last allocated cluster.
            if let Some(&last) = out.last() {
                *hint = last + 1;
                if *hint < 2 {
                    *hint = 2;
                }
            }
            self.free_clus_count
                .fetch_sub(count, Ordering::Relaxed);
            return Ok(out);
        }

        // Fallback: rotating linear scan over the FAT itself (no bitmap).
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..n {
            if out.len() >= count as usize {
                break;
            }
            let clus = 2 + ((*hint - 2 + i) % n);
            let val = match self.read_fat_entry(clus) {
                Ok(v) => v,
                Err(e) => {
                    for c in &out {
                        let _ = self.write_fat_entry(*c, FREE_CLUSTER);
                    }
                    return Err(e);
                }
            };
            if val == FREE_CLUSTER {
                if let Err(e) = self.write_fat_entry(clus, EOC_MARKER) {
                    for c in &out {
                        let _ = self.write_fat_entry(*c, FREE_CLUSTER);
                    }
                    return Err(e);
                }
                out.push(clus);
            }
        }
        if out.len() < count as usize {
            for c in &out {
                let _ = self.write_fat_entry(*c, FREE_CLUSTER);
            }
            return Err(VfsError::NoSpace);
        }
        if let Some(&last) = out.last() {
            *hint = last + 1;
        }
        self.free_clus_count
            .fetch_sub(count, Ordering::Relaxed);
        Ok(out)
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

        // Batched DMA: the FAT is contiguous on disk, so read it in
        // 252 KiB chunks (504 sectors) instead of 242+ single-sector AHCI
        // commands.  The bypass-cache path in CachedDevice::read_io lets the
        // AHCI PRDT DMA straight into the caller's buffer, collapsing ~242 IRQs
        // + PRDT builds into one.  Chunking keeps us under the 252 KiB / 64-entry
        // PRDT limit even for large volumes (16M clusters => ~125k FAT sectors).
        // Streaming per-chunk keeps peak allocation at 258 KiB instead of the
        // whole FAT (62 MiB for 16M clusters).
        const MAX_SECTORS_PER_IO: u64 = 504; // 252*1024 / 512
        if nsec == 0 {
            *self.alloc_bitmap.lock() = bitmap;
            return Ok(count);
        }
        let mut chunk_buf = alloc::vec![0u8; MAX_SECTORS_PER_IO as usize * SECTOR_SIZE];
        let mut done: u64 = 0;
        while done < nsec as u64 {
            let chunk = core::cmp::min(MAX_SECTORS_PER_IO, nsec as u64 - done);
            let len = (chunk as usize) * SECTOR_SIZE;
            read_sectors(
                &*self.device,
                first_lba + done,
                chunk as u32,
                &mut chunk_buf[..len],
            )?;
            for sec_off in 0..chunk {
                let sec = done + sec_off;
                let entry_base = sec * 128;
                let base = (sec_off as usize) * SECTOR_SIZE;
                for i in 0..128 {
                    let entry_idx = entry_base + i;
                    if entry_idx < first_valid || entry_idx > last_valid {
                        continue;
                    }
                    let off = base + (i * 4) as usize;
                    let val = u32::from_le_bytes(
                        chunk_buf
                            .get(off..off + 4)
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
            done += chunk;
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
