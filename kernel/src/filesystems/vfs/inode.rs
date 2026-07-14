use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

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
    pub append_lock: Mutex<()>,
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
            append_lock: Mutex::new(()),
        }
    }

    pub fn update_attr_from_stat(&self, stat: &Stat) {
        self.size.store(stat.size, Ordering::Relaxed);
        let mut meta = self.meta.lock();
        meta.mtime = stat.mtime;
    }
}
