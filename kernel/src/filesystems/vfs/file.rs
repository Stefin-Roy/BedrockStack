use alloc::sync::Arc;

use super::dentry::Dentry;
use super::inode::Inode;
use super::irq::IrqMutex;
use super::types::OpenFlags;

pub struct FileDescription {
    pub dentry: Arc<Dentry>,
    pub inode: Arc<Inode>,
    pub pos: IrqMutex<u64>,
    pub flags: OpenFlags,
}

impl FileDescription {
    pub fn new(dentry: Arc<Dentry>, inode: Arc<Inode>, flags: OpenFlags) -> Self {
        let initial_pos = if flags.contains(OpenFlags::APPEND) {
            inode.size.load(core::sync::atomic::Ordering::Relaxed)
        } else {
            0
        };
        FileDescription {
            dentry,
            inode,
            pos: IrqMutex::new(initial_pos),
            flags,
        }
    }
}
