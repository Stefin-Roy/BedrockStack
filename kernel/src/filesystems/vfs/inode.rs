use core::sync::atomic::{AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::error::VfsError;
use super::irq::IrqMutex;
use super::types::{DirEntry, FileType, Stat};

pub trait InodeOps: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError>;
    fn lookup(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError>;
    fn create(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError>;
    fn unlink(&self, name: &str) -> Result<(), VfsError>;
    fn mkdir(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError>;
    fn rmdir(&self, name: &str) -> Result<(), VfsError>;
    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError>;
    fn getattr(&self) -> Result<Stat, VfsError>;
    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), VfsError>;
    fn truncate(&self, len: u64) -> Result<(), VfsError>;
    fn file_type(&self) -> FileType;
    fn ino(&self) -> u64;
    fn size(&self) -> u64;

    /// Permission / time extensions — default is unsupported; tmpfs overrides.
    fn chmod(&self, _mode: u32) -> Result<(), VfsError> { Err(VfsError::NotSupported) }
    fn chown(&self, _uid: u32, _gid: u32) -> Result<(), VfsError> { Ok(()) }
    fn utimens(&self, _mtime: u64) -> Result<(), VfsError> { Err(VfsError::NotSupported) }
    fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<dyn InodeOps>, VfsError> { Err(VfsError::NotSupported) }
    fn readlink(&self) -> Result<String, VfsError> { Err(VfsError::NotSupported) }
    fn link(&self, _old: &str, _new: &str) -> Result<(), VfsError> { Err(VfsError::NotSupported) }
    fn mknod(&self, _name: &str, _mode: u32, _dev: u64) -> Result<Arc<dyn InodeOps>, VfsError> { Err(VfsError::NotSupported) }
    fn mkfifo(&self, _name: &str, _mode: u32) -> Result<Arc<dyn InodeOps>, VfsError> { Err(VfsError::NotSupported) }

    /// Canonical form of a child name used for cache keying (dentry tree +
    /// dcache).  Filesystems whose lookup is case-insensitive (FAT32, NTFS)
    /// MUST override this to fold case, otherwise `foo` and `FOO` resolve to
    /// the same dirent through two distinct cache identities.  Default:
    /// identity (case-sensitive filesystems like tmpfs).
    fn canonical_name(&self, name: &str) -> String {
        String::from(name)
    }

    /// Downcast hook for same-filesystem cooperation between two directory
    /// inodes (native cross-directory rename).  Default: None.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// Move `old_name` from `self` into `new_dir` as `new_name` without
    /// copying data.  Implementations must refuse when `new_dir` belongs to
    /// a different superblock (CrossDeviceLink); VFS never falls back to a
    /// byte-copy -- cross-mount renames are EXDEV, period.
    fn rename_across_dirs(
        &self,
        _new_dir: &dyn InodeOps,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), VfsError> {
        Err(VfsError::CrossDeviceLink)
    }

    /// Called by VFS before the inode is removed from the namespace (unlink,
    /// rmdir).  The filesystem can mark the inode for deferred cleanup — the
    /// inode won't be dropped until the last open handle is closed.
    fn on_unlink(&self) {}

    /// Durably flush filesystem state covering this file (O_SYNC write path).
    /// Default: no-op for filesystems with no write-back cache.
    fn flush(&self) -> Result<(), VfsError> {
        Ok(())
    }
}

pub struct InodeMeta {
    pub mtime: u64,
}

pub struct Inode {
    pub ops: Arc<dyn InodeOps>,
    pub ino: u64,
    pub file_type: FileType,
    pub size: AtomicU64,
    pub meta: IrqMutex<InodeMeta>,
    pub append_lock: IrqMutex<()>,
}

impl Inode {
    pub fn new(ops: Arc<dyn InodeOps>) -> Self {
        let ino = ops.ino();
        let file_type = ops.file_type();
        let size = ops.size();
        Inode {
            ops,
            ino,
            file_type,
            size: AtomicU64::new(size),
            meta: IrqMutex::new(InodeMeta { mtime: 0 }),
            append_lock: IrqMutex::new(()),
        }
    }

    pub fn update_attr_from_stat(&self, stat: &Stat) {
        self.size.store(stat.size, Ordering::Relaxed);
        let mut meta = self.meta.lock();
        meta.mtime = stat.mtime;
    }
}
