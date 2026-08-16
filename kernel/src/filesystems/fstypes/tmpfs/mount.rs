use alloc::sync::Arc;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::{Inode, InodeOps};
use crate::filesystems::vfs::superblock::{StatFs, SuperBlock, SuperOps};

use super::inode::TmpfsInode;
use crate::filesystems::fstypes::FileSystem;

/// Reported tmpfs capacity.  In-memory filesystems have no fixed backing
/// store, so the size limit is a documented budget against which usage is
/// tracked and reported (df-style consumers get real numbers).
pub const TMPFS_BUDGET: u64 = 64 * 1024 * 1024;

pub struct Tmpfs;

impl FileSystem for Tmpfs {
    fn name(&self) -> &str {
        "tmpfs"
    }

    fn mount(
        &self,
        _device: Option<Arc<dyn BlockDevice>>,
    ) -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError> {
        let used = Arc::new(core::sync::atomic::AtomicU64::new(0));
        let super_ops = Arc::new(TmpfsSuperOps { used: used.clone() });
        let root_ops = Arc::new(TmpfsInode::new_root(used)) as Arc<dyn InodeOps>;
        let root_inode = Arc::new(Inode::new(root_ops.clone()));
        let sb = Arc::new(SuperBlock::new(super_ops.clone(), root_inode));
        Ok((sb, root_ops))
    }
}

struct TmpfsSuperOps {
    used: Arc<core::sync::atomic::AtomicU64>,
}

impl SuperOps for TmpfsSuperOps {
    fn statfs(&self) -> Result<StatFs, VfsError> {
        use core::sync::atomic::Ordering;
        let used = self.used.load(Ordering::Relaxed);
        Ok(StatFs {
            block_size: 4096,
            total_blocks: TMPFS_BUDGET / 4096,
            free_blocks: TMPFS_BUDGET.saturating_sub(used) / 4096,
        })
    }
    fn sync_fs(&self) -> Result<(), VfsError> {
        Ok(())
    }
}
