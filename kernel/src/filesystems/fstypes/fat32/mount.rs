use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hashbrown::HashMap;
use crate::sync::PreemptMutex;

use crate::filesystems::blockdriver::block_cache::CachedDevice;
use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::fstypes::FileSystem;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::superblock::{StatFs, SuperBlock, SuperOps};
use crate::filesystems::vfs::types::FileType;

use super::bpb::{Bpb, SECTOR_SIZE, parse_bpb};
use super::cache::FatCache;
use super::io::read_sectors;

pub struct Fat32SuperBlock {
    pub(crate) device: Arc<dyn BlockDevice>,
    pub(crate) bpb: Bpb,
    pub(crate) fat_cache: PreemptMutex<FatCache>,
    pub(crate) next_ino: AtomicU64,
    /// Stable inode numbers keyed by (parent directory cluster, entry name).
    /// FAT has no inode table, so the identity is the namespace location.
    pub(crate) ino_map: PreemptMutex<HashMap<(u32, String), u64>>,
    /// Live inode handles keyed by (parent cluster, case-folded entry name).
    /// Lets unlink/rmdir distinguish "open handles exist -- defer cluster
    /// release to their Drop" from "orphaned name -- free the chain now",
    /// closing both the stale-handle dirent-stomp and the evicted-dentry
    /// cluster-leak holes.
    pub(crate) handle_registry: PreemptMutex<HashMap<(u32, String), Vec<Weak<super::inode::Fat32Inode>>>>,
    /// Shared chain metadata. Unispace resolves paths into fresh inode views,
    /// so a per-inode chain cache would be discarded after every syscall.
    pub(crate) chain_cache: PreemptMutex<HashMap<u32, Arc<Vec<u32>>>>,
    /// In-RAM allocation bitmap (built during the initial free-cluster scan).
    /// `None` on oversized volumes that use the FAT-scan allocator.
    pub(crate) alloc_bitmap: PreemptMutex<Option<super::alloc::AllocBitmap>>,
    pub(crate) next_alloc_hint: PreemptMutex<u32>,
    pub(crate) free_clus_count: AtomicU32,
    pub(crate) volume_dirty: AtomicBool,
}

impl Fat32SuperBlock {
    /// Return a file's cluster chain, building it once per first-cluster key.
    pub(crate) fn chain_for(&self, first: u32) -> Result<Arc<Vec<u32>>, VfsError> {
        if first < 2 || first >= super::fat::EOC_MARKER {
            return Ok(Arc::new(Vec::new()));
        }

        // Keep the cache lock while building. This prevents concurrent first
        // reads from walking the same large FAT chain and allocating duplicate
        // vectors.
        let cache = self.chain_cache.lock();
        if let Some(chain) = cache.get(&first) {
            return Ok(Arc::clone(chain));
        }
        drop(cache);

        // Batched FAT walk: for small FATs (ESP ~242 sectors) read the whole
        // FAT in one DMA to avoid 500+ single-sector AHCI cmds for a 28 MiB
        // WAD (56k clusters). For large FATs (>1024 sectors) fall back to
        // per-entry cached reads to avoid 64 MiB alloc.
        let total_clus = self.bpb.total_clus;
        let fat_sz = self.bpb.fat_sz32 as u64;
        let use_bulk = fat_sz <= 1024 && total_clus <= 200_000;
        let chain_vec = if use_bulk {
            let fat0_lba = self.bpb.fat_sector_lba(0, 0);
            let mut fat_data = alloc::vec![0u8; fat_sz as usize * super::bpb::SECTOR_SIZE];
            // Chunk to 504 sectors (252 KiB) to stay within 64-entry PRDT.
            const CHUNK: u64 = 504;
            let mut off = 0u64;
            while off < fat_sz {
                let c = core::cmp::min(CHUNK, fat_sz - off);
                let dst_off = (off as usize) * super::bpb::SECTOR_SIZE;
                let dst_end = dst_off + (c as usize) * super::bpb::SECTOR_SIZE;
                super::io::read_sectors(
                    &*self.device,
                    fat0_lba + off,
                    c as u32,
                    &mut fat_data[dst_off..dst_end],
                )?;
                off += c;
            }
            let mut chain = Vec::new();
            let mut c = first;
            let mut count: u32 = 0;
            loop {
                chain.push(c);
                let off = c as usize * 4;
                if off + 4 > fat_data.len() {
                    return Err(VfsError::IOError);
                }
                let next = u32::from_le_bytes([
                    fat_data[off],
                    fat_data[off + 1],
                    fat_data[off + 2],
                    fat_data[off + 3],
                ]) & 0x0FFFFFFF;
                if next >= super::fat::EOC_MARKER {
                    break;
                }
                if next < 2 || next >= 2 + total_clus {
                    return Err(VfsError::IOError);
                }
                c = next;
                count += 1;
                if count > total_clus + 2 {
                    return Err(VfsError::IOError);
                }
                // Safety: prevent infinite loop on corrupted FAT
                if count as usize > fat_data.len() {
                    return Err(VfsError::IOError);
                }
            }
            chain
        } else {
            let mut chain = Vec::new();
            let mut c = first;
            let mut count: u32 = 0;
            loop {
                chain.push(c);
                let next = self.read_fat_entry(c)?;
                if next >= super::fat::EOC_MARKER {
                    break;
                }
                c = next;
                count += 1;
                if count > total_clus + 2 {
                    return Err(VfsError::IOError);
                }
            }
            chain
        };

        let chain = Arc::new(chain_vec);
        // Re-lock to insert (another thread may have inserted meanwhile).
        let mut cache = self.chain_cache.lock();
        if let Some(existing) = cache.get(&first) {
            return Ok(Arc::clone(existing));
        }
        cache.insert(first, Arc::clone(&chain));
        Ok(chain)
    }

    /// Invalidate a chain before its FAT links are changed or its clusters
    /// are released. A zero first cluster has no cache entry.
    pub(crate) fn invalidate_chain(&self, first: u32) {
        if first != 0 {
            self.chain_cache.lock().remove(&first);
        }
    }

    /// Stable inode number for an entry.  Allocates on first sight so the same
    /// (parent, name) always maps to the same number, whichever VFS API asks.
    /// Names are case-folded to match the case-insensitive on-disk lookup, so
    /// `foo`/`FOO` share one inode identity.
    pub fn ino_for(&self, parent_clus: u32, name: &str) -> u64 {
        let canon = name.to_ascii_lowercase();
        let mut map = self.ino_map.lock();
        if let Some(&ino) = map.get(&(parent_clus, canon.clone())) {
            return ino;
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        map.insert((parent_clus, canon), ino);
        ino
    }

    fn registry_key(parent_clus: u32, name: &str) -> (u32, String) {
        (parent_clus, name.to_ascii_lowercase())
    }

    /// Register a live handle under its namespace location.  Prunes dead
    /// weaks on the way to keep the vec bounded.
    pub(crate) fn register_handle(
        &self,
        parent_clus: u32,
        name: &str,
        node: &Arc<super::inode::Fat32Inode>,
    ) {
        let key = Self::registry_key(parent_clus, name);
        let weak = Arc::downgrade(node);
        let mut reg = self.handle_registry.lock();
        let vec = reg.entry(key).or_default();
        vec.retain(|w| w.upgrade().is_some());
        vec.push(weak);
    }

    /// Mark every live handle under (parent, name) as unlinked so their Drop
    /// releases the cluster chain exactly once.  Returns the number of live
    /// handles found.  Zero means no one owns the chain anymore and the
    /// caller must free it itself.
    pub(crate) fn mark_handles_unlinked(&self, parent_clus: u32, name: &str) -> usize {
        let key = Self::registry_key(parent_clus, name);
        let mut reg = self.handle_registry.lock();
        if let Some(vec) = reg.remove(&key) {
            let mut live = 0usize;
            for w in vec {
                if let Some(node) = w.upgrade() {
                    node.unlinked.store(true, Ordering::Relaxed);
                    live += 1;
                }
            }
            return live;
        }
        0
    }

    /// Move all registered handles from one namespace location to another
    /// (rename).  Live handles keep their deferred-free obligation.
    pub(crate) fn move_handles(&self, old_parent: u32, old_name: &str, new_parent: u32, new_name: &str) {
        let from = Self::registry_key(old_parent, old_name);
        let to = Self::registry_key(new_parent, new_name);
        let mut reg = self.handle_registry.lock();
        let moved = reg.remove(&from).unwrap_or_default();
        let vec = reg.entry(to).or_default();
        vec.retain(|w| w.upgrade().is_some());
        vec.extend(moved.into_iter().filter(|w| w.upgrade().is_some()));
    }
    pub fn set_volume_dirty_flag(&self) -> Result<(), VfsError> {
        if self.volume_dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut sector = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, 0, 1, &mut sector)?;
        sector[0x41] |= 1;
        super::io::write_sectors(&*self.device, 0, 1, &sector)?;
        self.volume_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn clear_volume_dirty_flag(&self) -> Result<(), VfsError> {
        if !self.volume_dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut sector = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, 0, 1, &mut sector)?;
        sector[0x41] &= !1u8;
        super::io::write_sectors(&*self.device, 0, 1, &sector)?;
        self.volume_dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Runtime flush: push dirty FAT sectors and FSInfo.  Does not touch the
    /// volume-dirty flag — the volume stays marked dirty while it is in use.
    pub fn sync_all(&self) -> Result<(), VfsError> {
        let mut cache = self.fat_cache.lock();
        cache.flush(&*self.device)?;
        drop(cache);
        self.write_fsinfo()
    }

    /// Clean-unmount teardown: flush, then clear the volume-dirty flag.
    pub fn shutdown(&self) -> Result<(), VfsError> {
        self.sync_all()?;
        self.clear_volume_dirty_flag()
    }
}

impl SuperOps for Fat32SuperBlock {
    fn statfs(&self) -> Result<StatFs, VfsError> {
        Ok(StatFs {
            block_size: self.bpb.byts_per_clus,
            total_blocks: self.bpb.total_clus as u64,
            free_blocks: self.free_clus_count.load(Ordering::Relaxed) as u64,
        })
    }
    fn sync_fs(&self) -> Result<(), VfsError> {
        self.sync_all()
    }
    fn shutdown(&self) -> Result<(), VfsError> {
        Fat32SuperBlock::shutdown(self)
    }
}

pub struct Fat32FileSystem;

impl FileSystem for Fat32FileSystem {
    fn name(&self) -> &str {
        "fat32"
    }

    fn mount(
        &self,
        device: Option<Arc<dyn BlockDevice>>,
    ) -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError> {
        use super::fat::FSINFO_LEAD_SIG;
        use super::inode::Fat32Inode;
        use crate::filesystems::vfs::inode::Inode;

        let dev = device.ok_or(VfsError::InvalidDevice)?;
        let cached = CachedDevice::new(dev.clone());
        let bpb = parse_bpb(&*cached)?;

        {
            let mut sector = [0u8; SECTOR_SIZE];
            read_sectors(&*cached, 0, 1, &mut sector)?;
            if sector[0x41] & 1 != 0 {
                log::warn!("FAT32: volume was not cleanly unmounted (dirty bit set)");
            }
        }

        let sb = Arc::new(Fat32SuperBlock {
            device: cached,
            bpb: bpb.clone(),
            fat_cache: PreemptMutex::new(FatCache::new(&bpb)),
            next_ino: AtomicU64::new(2),
            ino_map: PreemptMutex::new(HashMap::new()),
            handle_registry: PreemptMutex::new(HashMap::new()),
            chain_cache: PreemptMutex::new(HashMap::new()),
            alloc_bitmap: PreemptMutex::new(None),
            next_alloc_hint: PreemptMutex::new(2),
            free_clus_count: AtomicU32::new(0),
            volume_dirty: AtomicBool::new(false),
        });

        // Mark the volume dirty for the whole session so an unclean shutdown
        // (crash/power loss) leaves the flag set for detection at next mount.
        sb.set_volume_dirty_flag()?;

        if bpb.fsinfo_is_valid() {
            let mut sector = [0u8; SECTOR_SIZE];
            if read_sectors(&*sb.device, bpb.fsinfo_sec as u64, 1, &mut sector).is_ok() {
                use super::fat::FSINFO_STRUCT_SIG;
                if sector[0..4] == FSINFO_LEAD_SIG.to_le_bytes()
                    && sector[484..488] == FSINFO_STRUCT_SIG.to_le_bytes()
                {
                    let hint =
                        u32::from_le_bytes([sector[492], sector[493], sector[494], sector[495]]);
                    if hint >= 2 && hint < 2 + bpb.total_clus {
                        *sb.next_alloc_hint.lock() = hint;
                    }
                }
            }
        }

        let free = sb.scan_free_clusters()?;
        sb.free_clus_count.store(free, Ordering::Relaxed);

        let root_clus = sb.bpb.root_clus;
        let root_ops = Arc::new(Fat32Inode {
            sb: sb.clone(),
            first_clus: AtomicU32::new(root_clus),
            size: AtomicU32::new(0),
            file_type: FileType::Directory,
            ino: 1,
            mtime: AtomicU64::new(0),
            parent_clus: root_clus,
            entry_name: String::new(),
            unlinked: AtomicBool::new(false),
            dir_cache: PreemptMutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: PreemptMutex::new(()),
            write_lock: crate::sync::PreemptRwLock::new(()),
        }) as Arc<dyn InodeOps>;

        let root_inode = Arc::new(Inode::new(root_ops.clone()));
        let super_ops = sb.clone() as Arc<dyn SuperOps>;
        let sb_vfs = Arc::new(SuperBlock::new(super_ops, root_inode));
        Ok((sb_vfs, root_ops))
    }
}
