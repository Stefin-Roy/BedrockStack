use alloc::sync::Arc;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::{Inode, InodeOps};
use crate::filesystems::vfs::superblock::{SuperBlock, SuperOps, StatFs};

use crate::filesystems::fstypes::FileSystem;
use super::inode::TmpfsInode;

pub struct Tmpfs;

impl FileSystem for Tmpfs {
    fn name(&self) -> &str { "tmpfs" }

    fn mount(&self, _device: Option<Arc<dyn BlockDevice>>)
             -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError>
    {
        let root_ops = Arc::new(TmpfsInode::new_root()) as Arc<dyn InodeOps>;
        let root_inode = Arc::new(Inode::new(root_ops.clone()));
        let super_ops = Arc::new(TmpfsSuperOps);
        let sb = Arc::new(SuperBlock::new(super_ops, root_inode));
        Ok((sb, root_ops))
    }
}

struct TmpfsSuperOps;

impl SuperOps for TmpfsSuperOps {
    fn statfs(&self) -> Result<StatFs, VfsError> {
        Ok(StatFs { block_size: 4096, total_blocks: 0, free_blocks: 0 })
    }
    fn sync_fs(&self) -> Result<(), VfsError> { Ok(()) }
}