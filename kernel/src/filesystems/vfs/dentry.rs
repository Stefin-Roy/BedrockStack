use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::{Arc, Weak};
use alloc::string::String;

use hashbrown::HashMap;
use spin::Once;

use super::inode::Inode;
use super::irq::IrqMutex;

const DCACHE_MAX: usize = 4096;

pub struct Dentry {
    pub name: IrqMutex<String>,
    pub inode: IrqMutex<Option<Arc<Inode>>>,
    pub parent: IrqMutex<Weak<Dentry>>,
    pub children: IrqMutex<HashMap<String, Arc<Dentry>>>,
    pub flags: AtomicBool,
}

impl Dentry {
    pub fn new(name: &str, inode: Option<Arc<Inode>>) -> Arc<Self> {
        Arc::new(Dentry {
            name: IrqMutex::new(String::from(name)),
            inode: IrqMutex::new(inode),
            parent: IrqMutex::new(Weak::new()),
            children: IrqMutex::new(HashMap::new()),
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
    map: spin::Mutex<HashMap<(u64, String), Weak<Dentry>>>,
}

static DCACHE: Once<Dcache> = Once::new();

pub fn dcache() -> &'static Dcache {
    DCACHE.call_once(|| Dcache {
        map: spin::Mutex::new(HashMap::new()),
    })
}

impl Dcache {
    pub fn lookup(&self, parent_ino: u64, name: &str) -> Option<Arc<Dentry>> {
        let map = self.map.lock();
        let key = (parent_ino, String::from(name));
        map.get(&key).and_then(|w| w.upgrade())
    }

    pub fn insert(&self, parent_ino: u64, name: String, dentry: Weak<Dentry>) {
        let mut map = self.map.lock();
        if map.len() >= DCACHE_MAX {
            if let Some(key) = map.keys().next().cloned() {
                map.remove(&key);
            }
        }
        map.insert((parent_ino, name), dentry);
    }

    pub fn evict(&self, parent_ino: u64, name: &str) {
        let mut map = self.map.lock();
        map.remove(&(parent_ino, String::from(name)));
    }
}
