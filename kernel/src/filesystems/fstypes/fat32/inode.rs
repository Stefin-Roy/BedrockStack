use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashSet;
use crate::sync::{PreemptMutex, PreemptRwLock};

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::types::{DirEntry, FileType, Stat};

use super::bpb::DIR_ENTRY_SIZE;
use super::bpb::MAX_SFN_LEN;
use super::cluster::{read_cluster, write_cluster, zero_cluster};
use super::dir::{
    decode_entry_name, read_dir_slots, remove_dir_entries, update_entry_cluster_and_size,
    write_dir_entries,
};
use super::dirent::{
    ATTR_ARCHIVE, ATTR_DIRECTORY, DirEntrySlot, MAX_LFN_ENTRIES, encode_vfat_entries,
    file_size_from_entry, first_clus_from_entry, lfn_slot_count, mtime_from_entry, needs_vfat,
    nt_case_flags, set_file_size_in_entry, set_first_clus_in_entry, set_timestamps, sfn_from_name,
    vfat_checksum,
};
use super::fat::EOC_MARKER;
use super::io::read_sectors;
use super::mount::Fat32SuperBlock;

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

pub struct Fat32Inode {
    pub(crate) sb: Arc<Fat32SuperBlock>,
    pub(crate) first_clus: AtomicU32,
    pub(crate) size: AtomicU32,
    pub(crate) file_type: FileType,
    pub(crate) ino: u64,
    /// Epoch-seconds modification time, mirrored from the directory entry's
    /// DOS write date/time (0 for the root, or before any timestamp is known).
    pub(crate) mtime: AtomicU64,
    pub(crate) parent_clus: u32,
    pub(crate) entry_name: String,
    pub(crate) unlinked: AtomicBool,
    pub(crate) dir_cache: PreemptMutex<Option<(u64, Arc<Vec<DirEntrySlot>>)>>,
    pub(crate) dir_generation: AtomicU64,
    pub(crate) dir_lock: PreemptMutex<()>,
    /// Serializes writes and truncates per inode.  Readers take the read
    /// half so concurrent reads of one file don't serialize each other;
    /// the FAT/cluster layers have their own sb-level locking.
    /// PreemptRwLock so a holder cannot be preempted and deadlock a spinner
    /// on the BSP (full preemption).
    pub(crate) write_lock: PreemptRwLock<()>,
}

impl Drop for Fat32Inode {
    fn drop(&mut self) {
        if !self.unlinked.load(Ordering::Relaxed) {
            return;
        }
        let first = self.first_clus.load(Ordering::Relaxed);
        if first >= 2 && first < EOC_MARKER {
            self.sb.invalidate_chain(first);
            let _ = self.sb.free_chain(first);
        }
    }
}

impl Fat32Inode {
    fn sync_clus_and_size(&self) -> Result<(), VfsError> {
        // A handle whose name was removed must never write metadata back:
        // the dirent may since belong to a recreated file, and stomping its
        // first_clus/size would corrupt the new file's data.
        if self.unlinked.load(Ordering::Relaxed) {
            return Ok(());
        }
        update_entry_cluster_and_size(
            &self.sb,
            self.parent_clus,
            &self.entry_name,
            Some(self.first_clus.load(Ordering::Relaxed)),
            Some(self.size.load(Ordering::Relaxed)),
        )
    }

    /// Dirent metadata update that silently no-ops once this handle's name
    /// has been unlinked (see sync_clus_and_size).
    fn update_dirent_meta(&self, clus: Option<u32>, size: Option<u32>) -> Result<(), VfsError> {
        if self.unlinked.load(Ordering::Relaxed) {
            return Ok(());
        }
        update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name, clus, size)
    }

    fn invalidate_dir_cache(&self) {
        self.dir_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn get_dir_slots(&self) -> Result<Arc<Vec<DirEntrySlot>>, VfsError> {
        let generation = self.dir_generation.load(Ordering::Relaxed);
        {
            let cache = self.dir_cache.lock();
            if let Some((g, ref slots)) = *cache {
                if g == generation {
                    return Ok(Arc::clone(slots));
                }
            }
        }
        fat_trace!(crate::drivers::serial::dump_puts(
            "[DBG:fat32] get_dir_slots re-read from disk\n"
        ));
        let first = self.first_clus.load(Ordering::Relaxed);
        fat_trace!({
            crate::drivers::serial::dump_puts("[DBG:fat32] get_dir_slots first_clus=0x");
            crate::drivers::serial::dump_put_hex(first as u64);
            crate::drivers::serial::dump_puts("\n");
        });
        let slots = Arc::new(read_dir_slots(&self.sb, first)?);
        fat_trace!({
            crate::drivers::serial::dump_puts("[DBG:fat32] get_dir_slots got ");
            crate::drivers::serial::dump_put_hex(slots.len() as u64);
            crate::drivers::serial::dump_puts(" slots\n");
        });
        *self.dir_cache.lock() = Some((generation, Arc::clone(&slots)));
        Ok(slots)
    }

    fn ensure_chain_cache(&self) -> Result<Arc<Vec<u32>>, VfsError> {
        let first = self.first_clus.load(Ordering::Relaxed);
        self.sb.chain_for(first)
    }
}

impl InodeOps for Fat32Inode {
    fn canonical_name(&self, name: &str) -> String {
        // On-disk matching is eq_ignore_ascii_case everywhere; cache keys must
        // fold identically or foo/FOO become two identities for one dirent.
        name.to_ascii_lowercase()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let _read_guard = self.write_lock.read();
        if self.file_type != FileType::Regular {
            return Err(VfsError::IsADirectory);
        }
        let file_size = self.size.load(Ordering::Relaxed) as u64;
        let first = self.first_clus.load(Ordering::Relaxed);
        if offset >= file_size || buf.is_empty() || first == 0 {
            return Ok(0);
        }

        let clus_size = self.sb.bpb.byts_per_clus as u64;
        let total = (buf.len() as u64).min(file_size - offset) as usize;
        let start_idx = (offset / clus_size) as u32;

        let chain = self.ensure_chain_cache()?;
        if chain.is_empty() {
            return Ok(0);
        }

        let clus_bytes = clus_size as usize;
        // Cap a single read request at 252 KiB so it always fits the AHCI
        // PRDT (64 entries x 4 KiB) even when the caller's buffer starts
        // mid-page and would otherwise need one extra entry.
        const MAX_RUN_BYTES: usize = 252 * 1024;

        let mut ci = start_idx as usize;
        let mut done = 0usize;
        let mut clus_off = (offset % clus_size) as usize;

        // For large sequential reads (28 MiB WAD) we batch multiple whole-run
        // DMAs into one AHCI NCQ submit (up to 16 slots) to collapse 100+ waits
        // into ~7. Misaligned head/tail still uses single run_buf path.
        let mut batch: alloc::vec::Vec<(u64, u32, usize, usize)> = alloc::vec::Vec::new();
        // Helper to flush pending whole-run batch via the central io:: batch primitive.
        // Keeps all AHCI submit logic in one place (io.rs) and avoids duplicating
        // IoRequest/unsafe reborrow in every caller.
        let flush_batch = |batch: &mut alloc::vec::Vec<(u64, u32, usize, usize)>,
                           device: &dyn crate::filesystems::blockdriver::traits::BlockDevice,
                           buf_ptr: *mut u8|
         -> Result<(), VfsError> {
            if batch.is_empty() {
                return Ok(());
            }
            let mut reqs: alloc::vec::Vec<(&mut [u8], u64, u32)> =
                alloc::vec::Vec::with_capacity(batch.len());
            for (lba, count, off, len) in batch.iter() {
                let slice = unsafe { core::slice::from_raw_parts_mut(buf_ptr.add(*off), *len) };
                reqs.push((slice, *lba, *count));
            }
            super::io::read_sectors_batch(device, &mut reqs)?;
            batch.clear();
            Ok(())
        };

        let buf_ptr = buf.as_mut_ptr();
        // Safety: buf_ptr lives for the whole read_at call; batch slices are
        // disjoint and derived from it, so no aliasing.
        while done < total && ci < chain.len() {
            // Grow a contiguous run while clusters are physically adjacent.
            let run_start = ci;
            let mut run_len = 1usize;
            while ci + run_len < chain.len() && chain[ci + run_len] == chain[ci + run_len - 1] + 1 {
                run_len += 1;
            }
            // Cap the run to the clusters we still need and to MAX_RUN_BYTES.
            let clusters_needed = (total - done + clus_off + clus_bytes - 1) / clus_bytes;
            run_len = run_len.min(clusters_needed.max(1));
            run_len = run_len.min((MAX_RUN_BYTES / clus_bytes).max(1));

            let run_bytes = run_len * clus_bytes;
            let avail = run_bytes - clus_off;
            let want = (total - done).min(avail);
            let run_lba = self.sb.bpb.cluster_to_lba(chain[run_start]);

            if clus_off == 0 && want == run_bytes {
                // Whole-run, sector-aligned: queue for batched DMA.
                batch.push((run_lba, (want / 512) as u32, done, want));
                done += want;
                ci += run_len;
                // Flush when batch full or next run would be misaligned/partial.
                let next_will_be_partial = {
                    if done >= total || ci >= chain.len() {
                        true
                    } else {
                        // Peek next run's want to see if it would be whole.
                        let next_clus_off = 0usize;
                        let next_clusters_needed =
                            (total - done + next_clus_off + clus_bytes - 1) / clus_bytes;
                        let mut next_len = 1usize;
                        while ci + next_len < chain.len()
                            && chain[ci + next_len] == chain[ci + next_len - 1] + 1
                        {
                            next_len += 1;
                        }
                        next_len = next_len.min(next_clusters_needed.max(1));
                        next_len = next_len.min((MAX_RUN_BYTES / clus_bytes).max(1));
                        let next_bytes = next_len * clus_bytes;
                        let next_want = (total - done).min(next_bytes);
                        !(next_want == next_bytes)
                    }
                };
                if batch.len() >= 16 || next_will_be_partial || done >= total {
                    flush_batch(&mut batch, &*self.sb.device, buf_ptr)?;
                }
            } else {
                // Misaligned head or partial tail: flush pending whole batch first.
                flush_batch(&mut batch, &*self.sb.device, buf_ptr)?;
                // Misaligned start or a partial tail run: read whole sectors
                // into a run-sized buffer, then copy the requested window.
                let mut run_buf = alloc::vec![0u8; run_bytes];
                read_sectors(
                    &*self.sb.device,
                    run_lba,
                    (run_bytes / 512) as u32,
                    &mut run_buf,
                )?;
                buf[done..done + want].copy_from_slice(&run_buf[clus_off..clus_off + want]);
                done += want;
                if done >= total {
                    break;
                }
                ci += run_len;
                clus_off = 0;
            }
        }
        // Flush any remaining whole-run batch.
        flush_batch(&mut batch, &*self.sb.device, buf_ptr)?;
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        if self.file_type != FileType::Regular {
            return Err(VfsError::IsADirectory);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if offset.saturating_add(buf.len() as u64) > u32::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        let _write_guard = self.write_lock.write();

        let clus_size = self.sb.bpb.byts_per_clus as u64;
        let end_byte = offset + buf.len() as u64;
        let needed_clus = if end_byte == 0 {
            0
        } else {
            ((end_byte - 1) / clus_size + 1) as u32
        };

        let mut current_first_clus = self.first_clus.load(Ordering::Relaxed);
        if needed_clus > 0 {
            let have = if current_first_clus == 0 {
                0
            } else {
                let chain = self.ensure_chain_cache()?;
                chain.len() as u32
            };
            if have < needed_clus {
                let need_new = needed_clus - have;
                if current_first_clus == 0 {
                    // Bulk allocate all clusters at once for contiguous runs.
                    let clusters = self.sb.alloc_clusters(need_new)?;
                    // Link the new chain internally (first already EOC, link rest).
                    for w in clusters.windows(2) {
                        self.sb.write_fat_entry(w[0], w[1])?;
                    }
                    current_first_clus = clusters[0];
                    // Zero new clusters, but skip those fully overwritten by this write
                    // (they will be DMA'd directly without prior zero). A cluster is
                    // fully overwritten iff its file byte range lies entirely inside
                    // [offset, offset+len).
                    let write_start = offset;
                    let write_end = offset + buf.len() as u64;
                    for (i, &c) in clusters.iter().enumerate() {
                        let global_idx = i;
                        let cs = global_idx as u64 * clus_size;
                        let ce = cs + clus_size;
                        if cs >= write_start && ce <= write_end {
                            continue;
                        }
                        if cs < write_end && ce > write_start {
                            continue;
                        }
                        zero_cluster(&self.sb, c)?;
                    }
                    self.first_clus.store(current_first_clus, Ordering::Relaxed);
                    self.sb.invalidate_chain(current_first_clus);
                    self.sync_clus_and_size()?;
                } else {
                    // Extend existing chain with bulk allocation.
                    let clusters = self.sb.alloc_clusters(need_new)?;
                    for w in clusters.windows(2) {
                        self.sb.write_fat_entry(w[0], w[1])?;
                    }
                    // Link tail to new head.
                    let tail = {
                        let chain = self.ensure_chain_cache()?;
                        chain[have as usize - 1]
                    };
                    self.sb.write_fat_entry(tail, clusters[0])?;
                    let write_start = offset;
                    let write_end = offset + buf.len() as u64;
                    for (i, &c) in clusters.iter().enumerate() {
                        let global_idx = have as usize + i;
                        let cs = global_idx as u64 * clus_size;
                        let ce = cs + clus_size;
                        if cs >= write_start && ce <= write_end {
                            continue;
                        }
                        // Partial overlap -> RMW will handle, skip zero.
                        if cs < write_end && ce > write_start {
                            continue;
                        }
                        zero_cluster(&self.sb, c)?;
                    }
                    self.sb.invalidate_chain(current_first_clus);
                }
            }
        }

        let start_idx = (offset / clus_size) as usize;
        let chain = self.ensure_chain_cache()?;
        if start_idx >= chain.len() && !buf.is_empty() {
            return Err(VfsError::IOError);
        }

        let clus_bytes = clus_size as usize;
        let sec_per_clus = self.sb.bpb.sec_per_clus as u32;
        let mut done = 0usize;
        let mut ci = start_idx;
        let mut clus_off = (offset % clus_size) as usize;

        // Batched write: coalesce contiguous whole-run writes into one AHCI NCQ submit
        // (up to 16 slots) just like read. Head/tail partials flush pending batch first.
        let mut write_batch: alloc::vec::Vec<(&[u8], u64, u32)> = alloc::vec::Vec::new();
        let flush_write_batch = |batch: &mut alloc::vec::Vec<(&[u8], u64, u32)>,
                                 device: &dyn crate::filesystems::blockdriver::traits::BlockDevice|
         -> Result<(), VfsError> {
            if batch.is_empty() {
                return Ok(());
            }
            super::io::write_sectors_batch(device, batch)?;
            batch.clear();
            Ok(())
        };

        while done < buf.len() && ci < chain.len() {
            // Find contiguous physical run length from ci.
            let run_start = ci;
            let mut run_len = 1usize;
            while run_start + run_len < chain.len()
                && chain[run_start + run_len] == chain[run_start + run_len - 1] + 1
                && run_start + run_len <= start_idx + (offset as usize + buf.len() + clus_bytes - 1) / clus_bytes
            {
                run_len += 1;
            }
            // Bound run_len to remaining clusters needed.
            let clusters_needed = (buf.len() - done + clus_off + clus_bytes - 1) / clus_bytes;
            run_len = run_len.min(clusters_needed);

            // Head partial cluster (unaligned start).
            if clus_off != 0 {
                flush_write_batch(&mut write_batch, &*self.sb.device)?;
                let cur = chain[ci];
                let mut cbuf = alloc::vec![0u8; clus_bytes];
                read_cluster(&self.sb, cur, &mut cbuf)?;
                let want = (buf.len() - done).min(clus_bytes - clus_off);
                cbuf[clus_off..clus_off + want].copy_from_slice(&buf[done..done + want]);
                write_cluster(&self.sb, cur, &cbuf)?;
                done += want;
                ci += 1;
                clus_off = 0;
                if done >= buf.len() {
                    break;
                }
                continue;
            }

            // Now aligned at run_start. Check how many whole clusters we can batch.
            let remaining = buf.len() - done;
            let max_whole = remaining / clus_bytes;
            // Whole clusters contiguous in this run.
            let whole_in_run = run_len.min(max_whole);
            if whole_in_run > 0 {
                // Queue for batched DMA instead of single write_sectors.
                let lba = self.sb.bpb.cluster_to_lba(chain[run_start]);
                let secs = whole_in_run as u32 * sec_per_clus;
                let slice = &buf[done..done + whole_in_run * clus_bytes];
                write_batch.push((slice, lba, secs));
                done += whole_in_run * clus_bytes;
                ci += whole_in_run;
                // Flush when batch full or next will be partial/tail.
                let next_will_be_partial = {
                    if done >= buf.len() || ci >= chain.len() {
                        true
                    } else {
                        let rem = buf.len() - done;
                        rem < clus_bytes
                    }
                };
                if write_batch.len() >= 16 || next_will_be_partial || done >= buf.len() {
                    flush_write_batch(&mut write_batch, &*self.sb.device)?;
                }
                if done >= buf.len() {
                    break;
                }
                continue;
            }

            // Tail partial (remaining < clus_bytes, aligned start but not whole).
            if remaining > 0 && remaining < clus_bytes {
                flush_write_batch(&mut write_batch, &*self.sb.device)?;
                let cur = chain[ci];
                let mut cbuf = alloc::vec![0u8; clus_bytes];
                // Only need RMW if cluster already has data (extending vs overwriting).
                // For clusters within existing file size, preserve tail bytes.
                // For newly allocated tail beyond old size, the cluster was zeroed, so no need to read.
                let file_size = self.size.load(Ordering::Relaxed) as u64;
                let cluster_file_end = (ci as u64 + 1) * clus_bytes as u64;
                if (ci as u64 * clus_bytes as u64) < file_size || cluster_file_end <= offset + remaining as u64 {
                    // Overwriting existing data or zeroed new cluster with partial write -> RMW to preserve unwritten tail.
                    if (ci as u64 * clus_bytes as u64) < file_size {
                        read_cluster(&self.sb, cur, &mut cbuf)?;
                    } else {
                        // New cluster beyond file size and not fully overwritten: already zeroed, keep zeros.
                    }
                }
                let want = remaining.min(clus_bytes);
                cbuf[0..want].copy_from_slice(&buf[done..done + want]);
                write_cluster(&self.sb, cur, &cbuf)?;
                break;
            }
        }
        // Flush any remaining whole-run batch.
        flush_write_batch(&mut write_batch, &*self.sb.device)?;

        self.sb.flush_fat_cache()?;

        let mut need_size_update = false;
        let cur_size = self.size.load(Ordering::Relaxed);
        let new_size = cur_size.max(end_byte as u32);
        if new_size > cur_size {
            self.size.store(new_size, Ordering::Relaxed);
            need_size_update = true;
        }

        if need_size_update || current_first_clus != self.first_clus.load(Ordering::Relaxed) {
            self.update_dirent_meta(None, Some(new_size))?;
        }

        self.mtime.store(
            crate::services::wallclock::now_epoch_secs().unwrap_or(0),
            Ordering::Relaxed,
        );

        Ok(buf.len())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        let slots: Arc<Vec<DirEntrySlot>> = if name == ".." {
            Arc::new(read_dir_slots(
                &self.sb,
                self.first_clus.load(Ordering::Relaxed),
            )?)
        } else {
            self.get_dir_slots()?
        };
        for slot in &*slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                let fc = first_clus_from_entry(&slot.sfn_entry);
                let actual_clus = if name == ".." && fc == 0 {
                    self.sb.bpb.root_clus
                } else {
                    fc
                };
                let sz = file_size_from_entry(&slot.sfn_entry);
                let attr = slot.sfn_entry[0x0B];
                let ft = if attr & ATTR_DIRECTORY != 0 {
                    FileType::Directory
                } else {
                    FileType::Regular
                };
                let node = Arc::new(Fat32Inode {
                    sb: self.sb.clone(),
                    first_clus: AtomicU32::new(actual_clus),
                    size: AtomicU32::new(sz),
                    file_type: ft,
                    ino: self
                        .sb
                        .ino_for(self.first_clus.load(Ordering::Relaxed), name),
                    mtime: AtomicU64::new(mtime_from_entry(&slot.sfn_entry)),
                    parent_clus: self.first_clus.load(Ordering::Relaxed),
                    entry_name: String::from(name),
                    unlinked: AtomicBool::new(false),
                    dir_cache: PreemptMutex::new(None),
                    dir_generation: AtomicU64::new(0),
                    dir_lock: PreemptMutex::new(()),
                    write_lock: PreemptRwLock::new(()),
                });
                if name != ".." {
                    self.sb
                        .register_handle(self.first_clus.load(Ordering::Relaxed), name, &node);
                }
                return Ok(node as Arc<dyn InodeOps>);
            }
        }
        Err(VfsError::NotFound)
    }

    fn create(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] create enter\n"
        ));
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        let _lock = self.dir_lock.lock();
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] create get_dir_slots\n"
        ));
        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] create got slots\n"
        ));
        if slots
            .iter()
            .any(|s| decode_entry_name(s).eq_ignore_ascii_case(name))
        {
            return Err(VfsError::AlreadyExists);
        }
        // LFN order fields only encode ordinals 1..=20; longer names cannot
        // be represented and must fail cleanly instead of writing garbage.
        if lfn_slot_count(name) > MAX_LFN_ENTRIES {
            return Err(VfsError::InvalidInput);
        }
        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots
            .iter()
            .map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                buf
            })
            .collect();
        let sfn = sfn_from_name(name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(name) {
            new_entries.extend(encode_vfat_entries(name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = ATTR_ARCHIVE;
        // Pure-SFN lowercase names keep their display form via NT flags.
        if !needs_vfat(name) {
            sfn_entry[0x0C] = nt_case_flags(name);
        }
        set_first_clus_in_entry(&mut sfn_entry, 0);
        set_file_size_in_entry(&mut sfn_entry, 0);
        let mtime = set_timestamps(&mut sfn_entry).unwrap_or(0);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] create write_dir_entries\n"
        ));
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] create done\n"
        ));
        self.invalidate_dir_cache();
        drop(_lock);

        let ino = self.sb.ino_for(parent, name);
        let node = Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: AtomicU32::new(0),
            size: AtomicU32::new(0),
            file_type: FileType::Regular,
            ino,
            mtime: AtomicU64::new(mtime),
            parent_clus: self.first_clus.load(Ordering::Relaxed),
            entry_name: String::from(name),
            unlinked: AtomicBool::new(false),
            dir_cache: PreemptMutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: PreemptMutex::new(()),
            write_lock: PreemptRwLock::new(()),
        });
        self.sb
            .register_handle(self.first_clus.load(Ordering::Relaxed), name, &node);
        Ok(node as Arc<dyn InodeOps>)
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] unlink enter\n"
        ));
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if name == "." || name == ".." {
            return Err(VfsError::InvalidInput);
        }
        let _lock = self.dir_lock.lock();

        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] unlink got slots\n"
        ));
        let mut found = false;
        let mut target_fc = 0u32;
        for slot in &*slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                if slot.sfn_entry[0x0B] & ATTR_DIRECTORY != 0 {
                    return Err(VfsError::IsADirectory);
                }
                target_fc = first_clus_from_entry(&slot.sfn_entry);
                found = true;
                break;
            }
        }
        if !found {
            return Err(VfsError::NotFound);
        }

        let parent = self.first_clus.load(Ordering::Relaxed);
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] unlink remove_dir_entries\n"
        ));
        remove_dir_entries(&self.sb, parent, name)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] unlink flush\n"
        ));
        self.sb.flush_fat_cache()?;
        self.invalidate_dir_cache();

        // Hand the deferred-free obligation to live handles, or -- when no
        // handle owns this chain (e.g. the dentry was evicted before the
        // VFS-level on_unlink could fire) -- release it right here so it
        // cannot leak.
        let live = self.sb.mark_handles_unlinked(parent, name);
        if live == 0 && target_fc >= 2 && target_fc < EOC_MARKER {
            self.sb.invalidate_chain(target_fc);
            self.sb.free_chain(target_fc)?;
            self.sb.flush_fat_cache()?;
        }
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] unlink done\n"
        ));
        Ok(())
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        let _lock = self.dir_lock.lock();
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir enter\n"
        ));
        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir got slots\n"
        ));
        if slots
            .iter()
            .any(|s| decode_entry_name(s).eq_ignore_ascii_case(name))
        {
            return Err(VfsError::AlreadyExists);
        }
        if lfn_slot_count(name) > MAX_LFN_ENTRIES {
            return Err(VfsError::InvalidInput);
        }

        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir alloc_cluster\n"
        ));
        let new_clus = self.sb.alloc_cluster()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir zero_cluster\n"
        ));
        zero_cluster(&self.sb, new_clus)?;

        let empty_set = HashSet::new();
        // SAFETY: "." and ".." are always valid SFN per `sfn_from_name` contract;
        // use `expect` with proof comment to avoid forbidden `unwrap` on device path.
        let dot_sfn = sfn_from_name(".", &empty_set).expect("BUG: '.' must be valid SFN");
        let dotdot_sfn = sfn_from_name("..", &empty_set).expect("BUG: '..' must be valid SFN");

        let mut dot_entry = [0u8; DIR_ENTRY_SIZE];
        dot_entry[..MAX_SFN_LEN].copy_from_slice(&dot_sfn);
        dot_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut dot_entry, new_clus);
        let _ = set_timestamps(&mut dot_entry);

        let mut dotdot_entry = [0u8; DIR_ENTRY_SIZE];
        dotdot_entry[..MAX_SFN_LEN].copy_from_slice(&dotdot_sfn);
        dotdot_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut dotdot_entry, self.first_clus.load(Ordering::Relaxed));
        let _ = set_timestamps(&mut dotdot_entry);

        let clus_bytes = self.sb.bpb.byts_per_clus as usize;
        let mut clus_buf = alloc::vec![0u8; clus_bytes];
        clus_buf[..DIR_ENTRY_SIZE].copy_from_slice(&dot_entry);
        clus_buf[DIR_ENTRY_SIZE..2 * DIR_ENTRY_SIZE].copy_from_slice(&dotdot_entry);
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir write_cluster\n"
        ));
        write_cluster(&self.sb, new_clus, &clus_buf)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir wrote dot/dotdot\n"
        ));

        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots
            .iter()
            .map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                buf
            })
            .collect();
        let sfn = sfn_from_name(name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(name) {
            new_entries.extend(encode_vfat_entries(name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut sfn_entry, new_clus);
        let mtime = set_timestamps(&mut sfn_entry).unwrap_or(0);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir write_dir_entries\n"
        ));
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir done\n"
        ));
        self.invalidate_dir_cache();
        drop(_lock);

        let ino = self.sb.ino_for(parent, name);
        let node = Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: AtomicU32::new(new_clus),
            size: AtomicU32::new(0),
            file_type: FileType::Directory,
            ino,
            mtime: AtomicU64::new(mtime),
            parent_clus: self.first_clus.load(Ordering::Relaxed),
            entry_name: String::from(name),
            unlinked: AtomicBool::new(false),
            dir_cache: PreemptMutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: PreemptMutex::new(()),
            write_lock: PreemptRwLock::new(()),
        });
        self.sb
            .register_handle(self.first_clus.load(Ordering::Relaxed), name, &node);
        Ok(node as Arc<dyn InodeOps>)
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if name == "." || name == ".." {
            return Err(VfsError::InvalidInput);
        }
        let _lock = self.dir_lock.lock();

        let slots = self.get_dir_slots()?;
        let mut target_clus = 0u32;
        let mut found = false;
        for slot in &*slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                let attr = slot.sfn_entry[0x0B];
                if attr & ATTR_DIRECTORY == 0 {
                    return Err(VfsError::NotADirectory);
                }
                target_clus = first_clus_from_entry(&slot.sfn_entry);
                found = true;
                break;
            }
        }
        if !found {
            return Err(VfsError::NotFound);
        }

        let child_slots = read_dir_slots(&self.sb, target_clus)?;
        if child_slots.len() > 2 {
            return Err(VfsError::NotEmpty);
        }

        let parent = self.first_clus.load(Ordering::Relaxed);
        remove_dir_entries(&self.sb, parent, name)?;
        self.sb.flush_fat_cache()?;
        self.invalidate_dir_cache();

        // Same orphan-vs-handle split as unlink: live handles free via Drop,
        // an unowned chain must not leak the directory's clusters.
        let live = self.sb.mark_handles_unlinked(parent, name);
        if live == 0 && target_clus >= 2 && target_clus < EOC_MARKER {
            self.sb.invalidate_chain(target_clus);
            self.sb.free_chain(target_clus)?;
            self.sb.flush_fat_cache()?;
        }
        Ok(())
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        fat_trace!(crate::drivers::serial::dump_puts(
            "[DBG:fat32] readdir enter\n"
        ));
        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::dump_puts(
            "[DBG:fat32] readdir got slots\n"
        ));
        let mut entries = Vec::with_capacity(slots.len());
        let dir_clus = self.first_clus.load(Ordering::Relaxed);
        for slot in &*slots {
            let name = decode_entry_name(slot);
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let attr = slot.sfn_entry[0x0B];
            let ft = if attr & ATTR_DIRECTORY != 0 {
                FileType::Directory
            } else {
                FileType::Regular
            };
            entries.push(DirEntry {
                ino: self.sb.ino_for(dir_clus, &name),
                name,
                file_type: ft,
            });
        }
        Ok(entries)
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        let mode = if self.file_type == FileType::Directory { 0o755 } else { 0o644 };
        Ok(Stat {
            ino: self.ino,
            size: if self.file_type == FileType::Directory {
                0
            } else {
                self.size.load(Ordering::Relaxed) as u64
            },
            file_type: self.file_type,
            mtime: self.mtime.load(Ordering::Relaxed),
            mode,
        })
    }

    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        let _lock = self.dir_lock.lock();

        let slots = self.get_dir_slots()?;
        let mut target = None;
        for slot in &*slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(old_name) {
                target = Some(slot);
                break;
            }
        }
        let target = target.ok_or(VfsError::NotFound)?;
        let fc = first_clus_from_entry(&target.sfn_entry);
        let sz = file_size_from_entry(&target.sfn_entry);
        let attr = target.sfn_entry[0x0B];
        let is_dir = (attr & ATTR_DIRECTORY) != 0;

        if let Some(existing) = slots
            .iter()
            .find(|s| decode_entry_name(s).eq_ignore_ascii_case(new_name))
        {
            let existing_clus = first_clus_from_entry(&existing.sfn_entry);
            // Handles open on the overwritten file lose their chain here;
            // mark them so their Drop does not free it a second time.
            let _ = self.sb.mark_handles_unlinked(
                self.first_clus.load(Ordering::Relaxed),
                new_name,
            );
            remove_dir_entries(&self.sb, self.first_clus.load(Ordering::Relaxed), new_name)?;
            self.sb.flush_fat_cache()?;
            if existing_clus >= 2 && existing_clus < EOC_MARKER {
                self.sb.invalidate_chain(existing_clus);
                self.sb.free_chain(existing_clus)?;
            }
            self.sb.flush_fat_cache()?;
            self.invalidate_dir_cache();
        }

        let target_sfn: [u8; MAX_SFN_LEN] = {
            let mut buf = [b' '; MAX_SFN_LEN];
            buf.copy_from_slice(&target.sfn_entry[..MAX_SFN_LEN]);
            buf
        };
        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots
            .iter()
            .filter_map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                if buf == target_sfn { None } else { Some(buf) }
            })
            .collect();
        let sfn = sfn_from_name(new_name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        if needs_vfat(new_name) && lfn_slot_count(new_name) > MAX_LFN_ENTRIES {
            return Err(VfsError::InvalidInput);
        }
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(new_name) {
            new_entries.extend(encode_vfat_entries(new_name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = attr;
        if !needs_vfat(new_name) {
            sfn_entry[0x0C] = nt_case_flags(new_name);
        }
        set_first_clus_in_entry(&mut sfn_entry, fc);
        set_file_size_in_entry(&mut sfn_entry, sz);
        let _ = set_timestamps(&mut sfn_entry);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        remove_dir_entries(&self.sb, parent, old_name)?;
        self.invalidate_dir_cache();
        // Live handles now belong to the new name; move their deferred-free
        // obligation so a later unlink(new_name) finds them.
        self.sb.move_handles(parent, old_name, parent, new_name);

        if is_dir && fc >= 2 && fc < EOC_MARKER && fc != self.sb.bpb.root_clus {
            self.sb.flush_fat_cache()?;
            let clus_bytes = self.sb.bpb.byts_per_clus as usize;
            let mut buf = alloc::vec![0u8; clus_bytes];
            read_cluster(&self.sb, fc, &mut buf)?;
            let dotdot_off = DIR_ENTRY_SIZE;
            set_first_clus_in_entry(
                buf.get_mut(dotdot_off..dotdot_off + DIR_ENTRY_SIZE)
                    .ok_or(VfsError::IOError)?
                    .try_into()
                    .map_err(|_| VfsError::IOError)?,
                parent,
            );
            write_cluster(&self.sb, fc, &buf)?;
        }

        Ok(())
    }

    fn truncate(&self, len: u64) -> Result<(), VfsError> {
        if self.file_type != FileType::Regular {
            return Err(VfsError::IsADirectory);
        }
        if len > u32::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        let _write_guard = self.write_lock.write();

        let new_size = len as u32;
        let clus_size = self.sb.bpb.byts_per_clus;
        let needed = if new_size == 0 {
            0
        } else {
            ((new_size as u64 - 1) / clus_size as u64 + 1) as u32
        };
        let current_first = self.first_clus.load(Ordering::Relaxed);
        let have = if current_first == 0 {
            0
        } else {
            self.ensure_chain_cache()?.len() as u32
        };

        if new_size == 0 && current_first != 0 {
            self.update_dirent_meta(Some(0), Some(0))?;
            self.sb.flush_fat_cache()?;
            self.sb.invalidate_chain(current_first);
            self.sb.free_chain(current_first)?;
            self.sb.flush_fat_cache()?;
            self.first_clus.store(0, Ordering::Relaxed);
            self.size.store(0, Ordering::Relaxed);
        } else if needed < have && current_first != 0 {
            self.sb.truncate_chain(current_first, needed)?;
            self.sb.flush_fat_cache()?;
            self.size.store(new_size, Ordering::Relaxed);
            self.sb.invalidate_chain(current_first);
            self.update_dirent_meta(None, Some(new_size))?;
        } else if needed > have {
            if current_first == 0 {
                let new_clus = self.sb.alloc_cluster()?;
                zero_cluster(&self.sb, new_clus)?;
                if needed > 1 {
                    self.sb.extend_chain(new_clus, needed - 1)?;
                }
                self.sb.flush_fat_cache()?;
                self.first_clus.store(new_clus, Ordering::Relaxed);
                self.size.store(new_size, Ordering::Relaxed);
                self.update_dirent_meta(Some(new_clus), Some(new_size))?;
                self.mtime.store(
                    crate::services::wallclock::now_epoch_secs().unwrap_or(0),
                    Ordering::Relaxed,
                );
                return Ok(());
            }
            self.sb.extend_chain(current_first, needed - have)?;
            self.sb.flush_fat_cache()?;
            self.size.store(new_size, Ordering::Relaxed);
            self.sb.invalidate_chain(current_first);
            self.update_dirent_meta(None, Some(new_size))?;
        } else {
            self.size.store(new_size, Ordering::Relaxed);
        }

        self.mtime.store(
            crate::services::wallclock::now_epoch_secs().unwrap_or(0),
            Ordering::Relaxed,
        );

        Ok(())
    }

    fn file_type(&self) -> FileType {
        self.file_type
    }
    fn ino(&self) -> u64 {
        self.ino
    }
    fn size(&self) -> u64 {
        if self.file_type == FileType::Directory {
            0
        } else {
            self.size.load(Ordering::Relaxed) as u64
        }
    }

    fn on_unlink(&self) {
        self.unlinked.store(true, Ordering::Relaxed);
    }

    fn flush(&self) -> Result<(), VfsError> {
        // FAT has no per-file flush primitive; push the volume's write-back
        // state (dirty FAT sectors + FSInfo).  Dirent/data sectors go through
        // the block layer synchronously already.
        self.sb.sync_all()
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn rename_across_dirs(
        &self,
        new_dir: &dyn InodeOps,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), VfsError> {
        let other = new_dir
            .as_any()
            .and_then(|a| a.downcast_ref::<Fat32Inode>())
            .ok_or(VfsError::CrossDeviceLink)?;
        if !Arc::ptr_eq(&self.sb, &other.sb) {
            return Err(VfsError::CrossDeviceLink);
        }
        if self.file_type != FileType::Directory || other.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return Err(VfsError::InvalidInput);
        }
        // Moving a directory onto itself is caught at the VFS dentry layer.

        // Lock both directories in deterministic cluster order so concurrent
        // renames cannot deadlock.  Guards are held for the entire
        // cross-directory sequence; releasing them early would allow a
        // concurrent rename to interleave and corrupt the dirents.
        let src_first = self.first_clus.load(Ordering::Relaxed);
        let dst_first = other.first_clus.load(Ordering::Relaxed);
        let _first_guard = if src_first <= dst_first {
            self.dir_lock.lock()
        } else {
            other.dir_lock.lock()
        };
        let _second_guard = if src_first != dst_first {
            Some(if src_first <= dst_first {
                other.dir_lock.lock()
            } else {
                self.dir_lock.lock()
            })
        } else {
            None
        };

        // Locate the source entry and capture its identity.
        let slots_src = self.get_dir_slots()?;
        let mut target: Option<&DirEntrySlot> = None;
        for slot in slots_src.iter() {
            if decode_entry_name(slot).eq_ignore_ascii_case(old_name) {
                target = Some(slot);
                break;
            }
        }
        let target = target.ok_or(VfsError::NotFound)?;
        let attr = target.sfn_entry[0x0B];
        let fc = first_clus_from_entry(&target.sfn_entry);
        let sz = file_size_from_entry(&target.sfn_entry);
        let is_dir = (attr & ATTR_DIRECTORY) != 0;

        // Destination collision: overwrite semantics matching same-dir rename.
        let slots_dst = other.get_dir_slots()?;
        if let Some(existing) = slots_dst
            .iter()
            .find(|s| decode_entry_name(s).eq_ignore_ascii_case(new_name))
        {
            let existing_clus = first_clus_from_entry(&existing.sfn_entry);
            let _ = self.sb.mark_handles_unlinked(dst_first, new_name);
            remove_dir_entries(&self.sb, dst_first, new_name)?;
            self.sb.flush_fat_cache()?;
            if existing_clus >= 2 && existing_clus < EOC_MARKER {
                self.sb.invalidate_chain(existing_clus);
                self.sb.free_chain(existing_clus)?;
            }
            other.invalidate_dir_cache();
        }

        // Build the destination group (fresh LFN checksum for the new name).
        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots_dst
            .iter()
            .map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                buf
            })
            .collect();
        let sfn = sfn_from_name(new_name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        if needs_vfat(new_name) && lfn_slot_count(new_name) > MAX_LFN_ENTRIES {
            return Err(VfsError::InvalidInput);
        }
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(new_name) {
            new_entries.extend(encode_vfat_entries(new_name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = attr;
        if !needs_vfat(new_name) {
            sfn_entry[0x0C] = nt_case_flags(new_name);
        }
        set_first_clus_in_entry(&mut sfn_entry, fc);
        set_file_size_in_entry(&mut sfn_entry, sz);
        let _ = set_timestamps(&mut sfn_entry);
        new_entries.push(sfn_entry);

        // Place the new group, then drop the source group.  Between these two
        // writes a crash can show the file in both dirs (both valid); it can
        // never show it in neither.
        write_dir_entries(&self.sb, &dst_first, &new_entries)?;
        remove_dir_entries(&self.sb, src_first, old_name)?;
        self.sb.flush_fat_cache()?;
        self.invalidate_dir_cache();
        other.invalidate_dir_cache();

        // A moved directory's ".." must point at its new parent.
        if is_dir && fc >= 2 && fc < EOC_MARKER && fc != self.sb.bpb.root_clus {
            let clus_bytes = self.sb.bpb.byts_per_clus as usize;
            let mut buf = alloc::vec![0u8; clus_bytes];
            read_cluster(&self.sb, fc, &mut buf)?;
            let dotdot_off = DIR_ENTRY_SIZE;
            set_first_clus_in_entry(
                buf.get_mut(dotdot_off..dotdot_off + DIR_ENTRY_SIZE)
                    .ok_or(VfsError::IOError)?
                    .try_into()
                    .map_err(|_| VfsError::IOError)?,
                dst_first,
            );
            write_cluster(&self.sb, fc, &buf)?;
        }

        self.sb.move_handles(src_first, old_name, dst_first, new_name);
        Ok(())
    }
}
