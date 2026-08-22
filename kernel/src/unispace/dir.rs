use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::filesystems::vfs::irq::IrqMutex;

use super::schema::{MethodDesc, Schema, Value};
use super::{DIR_SCHEMA, ListingEntry, Object, ObjectKind, UnispaceError};

// ── Namespace generation counter ───────────────────────────────────────
//
// Bumped on every SimpleDir child insert/remove. The resolution cache stamps
// entries with the generation at resolve time; any structural mutation
// anywhere in a SimpleDir-backed chain invalidates every cached resolution
// in one load-compare. VFS/proc providers do not participate (their objects
// are not `cacheable()`), so their mutations need no bump.
static TREE_GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Current namespace generation (see module-level comment in `dir.rs`).
pub fn tree_gen() -> u64 {
    TREE_GEN.load(core::sync::atomic::Ordering::Acquire)
}

fn bump_tree_gen() {
    TREE_GEN.fetch_add(1, core::sync::atomic::Ordering::Release);
}

/// A provider-side map-backed directory.  The unispace root `/` is one of
/// these; providers may instantiate their own for static subtrees.  This is a
/// provider utility — unispace core owns no tree state beyond the root itself.
pub struct SimpleDir {
    children: IrqMutex<HashMap<String, Arc<dyn Object>>>,
}

impl SimpleDir {
    pub fn new() -> Self {
        SimpleDir {
            children: IrqMutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, name: &str, obj: Arc<dyn Object>) {
        self.children.lock().insert(String::from(name), obj);
        bump_tree_gen();
    }

    pub fn remove(&self, name: &str) -> bool {
        let removed = self.children.lock().remove(name).is_some();
        if removed {
            bump_tree_gen();
        }
        removed
    }
}

impl Object for SimpleDir {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Dir
    }

    fn value_schema(&self) -> &'static Schema {
        &DIR_SCHEMA
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        self.children.lock().get(name).cloned()
    }

    fn cacheable(&self) -> bool {
        true
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        let children = self.children.lock();
        let mut keys: Vec<&String> = children.keys().collect();
        keys.sort();
        for k in keys {
            // Index once, no re-probe fallback needed — `k` came from the map.
            out.push(ListingEntry {
                name: k.clone(),
                kind: children[k].kind(),
            });
        }
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut entries = Vec::new();
        self.list(&mut entries)?;
        super::encode_listing(entries, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }

    fn invoke(&self, _method: usize, _v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        Err(UnispaceError::MethodNotFound)
    }

    fn insert_child(&self, name: &str, obj: Arc<dyn Object>) -> Result<(), UnispaceError> {
        self.insert(name, obj);
        Ok(())
    }

    fn remove_child(&self, name: &str) -> Result<bool, UnispaceError> {
        Ok(self.remove(name))
    }
}
