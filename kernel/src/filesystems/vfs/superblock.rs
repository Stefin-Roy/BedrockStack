use alloc::sync::Arc;

use super::error::VfsError;
use super::inode::Inode;

pub trait SuperOps: Send + Sync {
    fn statfs(&self) -> Result<StatFs, VfsError>;
    fn sync_fs(&self) -> Result<(), VfsError>;
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
