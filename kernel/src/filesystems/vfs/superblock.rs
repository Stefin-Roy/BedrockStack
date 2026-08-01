use alloc::sync::Arc;

use super::error::VfsError;
use super::inode::Inode;

pub trait SuperOps: Send + Sync {
    fn statfs(&self) -> Result<StatFs, VfsError>;
    fn sync_fs(&self) -> Result<(), VfsError>;

    /// Called by `vfs::unmount` after `sync_fs()`.  A filesystem can use this
    /// to perform teardown that must only happen on a clean unmount (e.g.
    /// clearing the FAT volume-dirty flag), as opposed to a plain runtime
    /// flush.  Defaults to `sync_fs` so existing superblocks keep working.
    fn shutdown(&self) -> Result<(), VfsError> {
        self.sync_fs()
    }
}

pub struct SuperBlock {
    pub ops: Arc<dyn SuperOps>,
    pub root_inode: Arc<Inode>,
}

impl SuperBlock {
    pub fn new(ops: Arc<dyn SuperOps>, root_inode: Arc<Inode>) -> Self {
        SuperBlock { ops, root_inode }
    }
}

pub struct StatFs {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
}
