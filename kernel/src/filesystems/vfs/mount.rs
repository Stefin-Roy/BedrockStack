use alloc::sync::{Arc, Weak};

use crate::filesystems::blockdriver::traits::BlockDevice;

use super::dentry::Dentry;
use super::irq::IrqMutex;
use super::superblock::SuperBlock;

static NEXT_MOUNT_ID: IrqMutex<u64> = IrqMutex::new(1);

pub fn next_mount_id() -> u64 {
    let mut id = NEXT_MOUNT_ID.lock();
    let val = *id;
    *id += 1;
    val
}

pub struct DriveMount {
    pub id: u64,
    pub root: Arc<Dentry>,
    pub sb: Arc<SuperBlock>,
    pub device: Option<Arc<dyn BlockDevice>>,
    pub covered: IrqMutex<Option<Weak<Dentry>>>,
}

impl DriveMount {
    pub fn new(id: u64, root: Arc<Dentry>, sb: Arc<SuperBlock>, device: Option<Arc<dyn BlockDevice>>) -> Self {
        DriveMount { id, root, sb, device, covered: IrqMutex::new(None) }
    }
}
