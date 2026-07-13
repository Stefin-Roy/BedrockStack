use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use alloc::string::String;

use hashbrown::HashMap;

use super::inode::Inode;
use super::irq::IrqMutex;

pub struct Dentry {
    pub name: String,
    pub inode: IrqMutex<Option<Arc<Inode>>>,
    pub parent: IrqMutex<Weak<Dentry>>,
    pub children: IrqMutex<Vec<Arc<Dentry>>>,
    pub flags: AtomicBool,
}

impl Dentry {
    pub fn new(name: &str, inode: Option<Arc<Inode>>) -> Arc<Self> {
        Arc::new(Dentry {
            name: String::from(name),
            inode: IrqMutex::new(inode),
            parent: IrqMutex::new(Weak::new()),
            children: IrqMutex::new(Vec::new()),
            flags: AtomicBool::new(false),
        })
    }

    pub fn is_negative(&self) -> bool {
        self.inode.lock().is_none()
    }

    pub fn is_mount_point(&self) -> bool {
        self.flags.load(Ordering::Relaxed)
    }

    pub fn set_mount_point(&self, val: bool) {
        self.flags.store(val, Ordering::Relaxed);
    }
}

pub struct Dcache {
    map: IrqMutex<HashMap<(u64, String), Weak<Dentry>>>,
}

impl Dcache {
    pub const fn new() -> Self {
        Dcache {
            map: IrqMutex::new(HashMap::new()),
        }
    }

    pub fn lookup(&self, parent_ino: u64, name: &str) -> Option<Arc<Dentry>> {
        let map = self.map.lock();
        map.get(&(parent_ino, String::from(name)))
            .and_then(|w| w.upgrade())
    }

    pub fn insert(&self, parent_ino: u64, name: String, dentry: Weak<Dentry>) {
        let mut map = self.map.lock();
        map.insert((parent_ino, name), dentry);
    }

    pub fn remove(&self, parent_ino: u64, name: &str) {
        let mut map = self.map.lock();
        map.remove(&(parent_ino, String::from(name)));
    }

    pub fn evict(&self, parent_ino: u64, name: &str) {
        let mut map = self.map.lock();
        map.remove(&(parent_ino, String::from(name)));
    }
}
