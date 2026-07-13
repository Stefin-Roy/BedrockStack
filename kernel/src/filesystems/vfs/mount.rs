use alloc::sync::Arc;

use crate::filesystems::blockdriver::traits::BlockDevice;

use super::dentry::Dentry;
use super::superblock::SuperBlock;

pub struct DriveMount {
    pub id: u64,
    pub root: Arc<Dentry>,
    pub sb: Arc<SuperBlock>,
    pub device: Option<Arc<dyn BlockDevice>>,
}

impl DriveMount {
    pub fn new(id: u64, root: Arc<Dentry>, sb: Arc<SuperBlock>, device: Option<Arc<dyn BlockDevice>>) -> Self {
        DriveMount { id, root, sb, device }
    }
}
