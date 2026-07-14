use alloc::sync::Arc;
use spin::Mutex;
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::superblock::SuperBlock;

pub mod tmpfs;
pub mod fat32;

pub trait FileSystem: Send + Sync {
    fn name(&self) -> &str;
    fn mount(&self, device: Option<Arc<dyn BlockDevice>>)
        -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError>;
}

static REGISTRY: Mutex<Vec<&'static dyn FileSystem>> = Mutex::new(Vec::new());

pub fn register(fs: &'static dyn FileSystem) {
    REGISTRY.lock().push(fs);
}

pub fn lookup(name: &str) -> Option<&'static dyn FileSystem> {
    REGISTRY.lock().iter().find(|fs| fs.name() == name).copied()
}

pub fn register_all() {
    register(&tmpfs::Tmpfs);
    register(&fat32::Fat32FileSystem);
}
