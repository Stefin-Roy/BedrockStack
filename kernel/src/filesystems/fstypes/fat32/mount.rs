use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use spin::Mutex;

use crate::filesystems::blockdriver::block_cache::CachedDevice;
use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::superblock::{SuperBlock, SuperOps, StatFs};
use crate::filesystems::vfs::types::FileType;
use crate::filesystems::fstypes::FileSystem;

use super::bpb::{Bpb, parse_bpb, SECTOR_SIZE};
use super::cache::FatCache;
use super::io::read_sectors;

pub struct Fat32SuperBlock {
    pub(crate) device: Arc<dyn BlockDevice>,
    pub(crate) bpb: Bpb,
    pub(crate) fat_cache: Mutex<FatCache>,
    pub(crate) next_ino: AtomicU64,
    pub(crate) next_alloc_hint: Mutex<u32>,
    pub(crate) free_clus_count: AtomicU32,
    pub(crate) volume_dirty: AtomicBool,
}

impl Fat32SuperBlock {
    pub fn set_volume_dirty_flag(&self) -> Result<(), VfsError> {
        if self.volume_dirty.load(Ordering::Relaxed) { return Ok(()); }
        let mut sector = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, 0, 1, &mut sector)?;
        sector[0x41] |= 1;
        super::io::write_sectors(&*self.device, 0, 1, &sector)?;
        self.volume_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn clear_volume_dirty_flag(&self) -> Result<(), VfsError> {
        if !self.volume_dirty.load(Ordering::Relaxed) { return Ok(()); }
        let mut sector = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, 0, 1, &mut sector)?;
        sector[0x41] &= !1u8;
        super::io::write_sectors(&*self.device, 0, 1, &sector)?;
        self.volume_dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn sync_all(&self) -> Result<(), VfsError> {
        self.set_volume_dirty_flag()?;
        let mut cache = self.fat_cache.lock();
        cache.flush(&*self.device, &self.bpb)?;
        drop(cache);
        self.write_fsinfo()?;
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
}

pub struct Fat32FileSystem;

impl FileSystem for Fat32FileSystem {
    fn name(&self) -> &str { "fat32" }

    fn mount(&self, device: Option<Arc<dyn BlockDevice>>)
             -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError>
    {
        use crate::filesystems::vfs::inode::Inode;
        use super::inode::Fat32Inode;
        use super::fat::FSINFO_LEAD_SIG;

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
            fat_cache: Mutex::new(FatCache::new()),
            next_ino: AtomicU64::new(2),
            next_alloc_hint: Mutex::new(2),
            free_clus_count: AtomicU32::new(0),
            volume_dirty: AtomicBool::new(false),
        });

        if bpb.fsinfo_is_valid() {
            let mut sector = [0u8; SECTOR_SIZE];
            if read_sectors(&*sb.device, bpb.fsinfo_sec as u64, 1, &mut sector).is_ok() {
                use super::fat::FSINFO_STRUCT_SIG;
                if sector[0..4] == FSINFO_LEAD_SIG.to_le_bytes()
                    && sector[484..488] == FSINFO_STRUCT_SIG.to_le_bytes()
                {
                    let hint = u32::from_le_bytes([sector[492], sector[493], sector[494], sector[495]]);
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
            parent_clus: root_clus,
            entry_name: String::new(),
            unlinked: AtomicBool::new(false),
            dir_cache: Mutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        }) as Arc<dyn InodeOps>;

        let root_inode = Arc::new(Inode::new(root_ops.clone()));
        let super_ops = sb.clone() as Arc<dyn SuperOps>;
        let sb_vfs = Arc::new(SuperBlock::new(super_ops, root_inode));
        Ok((sb_vfs, root_ops))
    }
}