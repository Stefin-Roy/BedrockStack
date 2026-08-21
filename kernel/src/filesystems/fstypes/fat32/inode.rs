use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashSet;
use spin::Mutex;

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
    ATTR_ARCHIVE, ATTR_DIRECTORY, DirEntrySlot, encode_vfat_entries, file_size_from_entry,
    first_clus_from_entry, mtime_from_entry, needs_vfat, set_file_size_in_entry,
    set_first_clus_in_entry, set_timestamps, sfn_from_name, vfat_checksum,
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
    pub(crate) dir_cache: Mutex<Option<(u64, Arc<Vec<DirEntrySlot>>)>>,
    pub(crate) dir_generation: AtomicU64,
    pub(crate) dir_lock: Mutex<()>,
    pub(crate) write_lock: Mutex<()>,
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
        update_entry_cluster_and_size(
            &self.sb,
            self.parent_clus,
            &self.entry_name,
            Some(self.first_clus.load(Ordering::Relaxed)),
            Some(self.size.load(Ordering::Relaxed)),
        )
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
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let _write_lock = self.write_lock.lock();
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
                // Whole-run, sector-aligned: DMA straight into the caller's
                // buffer, no intermediate copy.
                read_sectors(
                    &*self.sb.device,
                    run_lba,
                    (want / 512) as u32,
                    &mut buf[done..done + want],
                )?;
            } else {
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
            }
            done += want;
            if done >= total {
                break;
            }
            ci += run_len;
            clus_off = 0;
        }
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
        let _write_lock = self.write_lock.lock();

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
                if current_first_clus == 0 {
                    current_first_clus = self.sb.alloc_cluster()?;
                    zero_cluster(&self.sb, current_first_clus)?;
                    if needed_clus > 1 {
                        self.sb.extend_chain(current_first_clus, needed_clus - 1)?;
                    }
                    self.first_clus.store(current_first_clus, Ordering::Relaxed);
                    self.sb.invalidate_chain(current_first_clus);
                    self.sync_clus_and_size()?;
                } else {
                    self.sb
                        .extend_chain(current_first_clus, needed_clus - have)?;
                    self.sb.invalidate_chain(current_first_clus);
                }
            }
        }

        let start_idx = (offset / clus_size) as u32;
        let chain = self.ensure_chain_cache()?;
        let mut ci = start_idx as usize;
        let mut current = if ci < chain.len() {
            chain[ci]
        } else {
            return Err(VfsError::IOError);
        };

        let clus_bytes = clus_size as usize;
        let mut cluster_buf = alloc::vec![0u8; clus_bytes];
        let mut done = 0usize;
        let mut clus_off = (offset % clus_size) as usize;

        while done < buf.len() {
            let need_rmw =
                clus_off != 0 || buf.len() - done < clus_bytes || (done > 0 && clus_off == 0);
            if need_rmw {
                read_cluster(&self.sb, current, &mut cluster_buf)?;
            } else {
                cluster_buf = alloc::vec![0u8; clus_bytes];
            }
            let avail = clus_bytes - clus_off;
            let want = (buf.len() - done).min(avail);
            cluster_buf[clus_off..clus_off + want].copy_from_slice(&buf[done..done + want]);
            write_cluster(&self.sb, current, &cluster_buf)?;
            done += want;
            if done >= buf.len() {
                break;
            }
            ci += 1;
            if ci >= chain.len() {
                break;
            }
            current = chain[ci];
            clus_off = 0;
        }

        self.sb.flush_fat_cache()?;

        let mut need_size_update = false;
        let cur_size = self.size.load(Ordering::Relaxed);
        let new_size = cur_size.max(end_byte as u32);
        if new_size > cur_size {
            self.size.store(new_size, Ordering::Relaxed);
            need_size_update = true;
        }

        if need_size_update || current_first_clus != self.first_clus.load(Ordering::Relaxed) {
            update_entry_cluster_and_size(
                &self.sb,
                self.parent_clus,
                &self.entry_name,
                None,
                Some(new_size),
            )?;
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
                return Ok(Arc::new(Fat32Inode {
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
                    dir_cache: Mutex::new(None),
                    dir_generation: AtomicU64::new(0),
                    dir_lock: Mutex::new(()),
                    write_lock: Mutex::new(()),
                }) as Arc<dyn InodeOps>);
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
        Ok(Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: AtomicU32::new(0),
            size: AtomicU32::new(0),
            file_type: FileType::Regular,
            ino,
            mtime: AtomicU64::new(mtime),
            parent_clus: self.first_clus.load(Ordering::Relaxed),
            entry_name: String::from(name),
            unlinked: AtomicBool::new(false),
            dir_cache: Mutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        }) as Arc<dyn InodeOps>)
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
        for slot in &*slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                if slot.sfn_entry[0x0B] & ATTR_DIRECTORY != 0 {
                    return Err(VfsError::IsADirectory);
                }
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

        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir alloc_cluster\n"
        ));
        let new_clus = self.sb.alloc_cluster()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] mkdir zero_cluster\n"
        ));
        zero_cluster(&self.sb, new_clus)?;

        let empty_set = HashSet::new();
        let dot_sfn = sfn_from_name(".", &empty_set).unwrap();
        let dotdot_sfn = sfn_from_name("..", &empty_set).unwrap();

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
        Ok(Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: AtomicU32::new(new_clus),
            size: AtomicU32::new(0),
            file_type: FileType::Directory,
            ino,
            mtime: AtomicU64::new(mtime),
            parent_clus: self.first_clus.load(Ordering::Relaxed),
            entry_name: String::from(name),
            unlinked: AtomicBool::new(false),
            dir_cache: Mutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        }) as Arc<dyn InodeOps>)
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
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(new_name) {
            new_entries.extend(encode_vfat_entries(new_name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = attr;
        set_first_clus_in_entry(&mut sfn_entry, fc);
        set_file_size_in_entry(&mut sfn_entry, sz);
        let _ = set_timestamps(&mut sfn_entry);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        remove_dir_entries(&self.sb, parent, old_name)?;
        self.invalidate_dir_cache();

        if is_dir && fc >= 2 && fc < EOC_MARKER && fc != self.sb.bpb.root_clus {
            self.sb.flush_fat_cache()?;
            let clus_bytes = self.sb.bpb.byts_per_clus as usize;
            let mut buf = alloc::vec![0u8; clus_bytes];
            read_cluster(&self.sb, fc, &mut buf)?;
            let dotdot_off = DIR_ENTRY_SIZE;
            set_first_clus_in_entry(
                &mut buf[dotdot_off..dotdot_off + DIR_ENTRY_SIZE]
                    .try_into()
                    .unwrap(),
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
        let _write_lock = self.write_lock.lock();

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
            update_entry_cluster_and_size(
                &self.sb,
                self.parent_clus,
                &self.entry_name,
                Some(0),
                Some(0),
            )?;
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
            update_entry_cluster_and_size(
                &self.sb,
                self.parent_clus,
                &self.entry_name,
                None,
                Some(new_size),
            )?;
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
                update_entry_cluster_and_size(
                    &self.sb,
                    self.parent_clus,
                    &self.entry_name,
                    Some(new_clus),
                    Some(new_size),
                )?;
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
            update_entry_cluster_and_size(
                &self.sb,
                self.parent_clus,
                &self.entry_name,
                None,
                Some(new_size),
            )?;
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
}
