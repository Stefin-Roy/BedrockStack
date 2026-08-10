use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Arc;
use spin::Mutex;

use super::irq::IrqMutex;
use super::types::{FileType, Stat};

// Re-export the unified trait from file_ops.rs. `FileOps` is the canonical
// name; `Inode` wraps an `Arc<dyn FileOps>`.
pub use super::file_ops::{FileOps, OpDesc};

pub struct InodeMeta {
    pub mtime: u64,
}

pub struct Inode {
    pub ops: Arc<dyn FileOps>,
    pub ino: u64,
    pub file_type: FileType,
    pub size: AtomicU64,
    pub meta: IrqMutex<InodeMeta>,
    pub append_lock: Mutex<()>,
}

impl Inode {
    pub fn new(ops: Arc<dyn FileOps>) -> Self {
        let ino = ops.ino();
        let file_kind = ops.file_kind();
        let size = ops.size();
        Inode {
            ops,
            ino,
            file_type: file_kind,
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
