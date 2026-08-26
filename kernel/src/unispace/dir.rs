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

/// A hybrid object that presents a Service value while also hosting children.
///
/// The `service` provides the value/methods (read/write/invoke) and determines
/// `kind`, while `inner` hosts child entries. This lets a leaf Service such as
/// `/kernel/mm/heap` stay a Service for `read()` yet still be traversed to
/// `/kernel/mm/heap/chunks`. Without this, a `connect()` to a child of a
/// Service would hit `Unsupported` and `resolve_parsed` would return
/// `NotADirectory`. `Dir` listing of this object returns the children, not the
/// service listing — `read()` returns the service value, so discovery of
/// children is via `read(parent)`'s listing of the parent dir, not this node.
pub struct ServiceDir {
    inner: SimpleDir,
    service: Arc<dyn Object>,
}

impl ServiceDir {
    pub fn new(service: Arc<dyn Object>) -> Self {
        ServiceDir { inner: SimpleDir::new(), service }
    }
}

impl Object for ServiceDir {
    fn kind(&self) -> ObjectKind {
        self.service.kind()
    }
    fn value_schema(&self) -> &'static Schema {
        self.service.value_schema()
    }
    fn owned_value_schema(&self) -> Option<&super::schema::OwnedSchema> {
        self.service.owned_value_schema()
    }
    fn methods(&self) -> &'static [MethodDesc] {
        self.service.methods()
    }
    fn owned_methods(&self) -> &[super::OwnedMethodDesc] {
        self.service.owned_methods()
    }
    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        self.inner.resolve(name)
    }
    fn cacheable(&self) -> bool {
        // service value may be dynamic, do not cache
        false
    }
    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        self.inner.list(out)
    }
    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        self.service.read_value(out, max)
    }
    fn read_value_flags(&self, out: &mut Vec<u8>, max: usize, flags: u64) -> Result<(), UnispaceError> {
        self.service.read_value_flags(out, max, flags)
    }
    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        self.service.write_value(v)
    }
    fn write_value_flags(&self, v: Value, flags: u64) -> Result<(), UnispaceError> {
        self.service.write_value_flags(v, flags)
    }
    fn write_blob_flags(&self, data: &[u8], flags: u64) -> Option<Result<(), UnispaceError>> {
        self.service.write_blob_flags(data, flags)
    }
    fn invoke(&self, method: usize, v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        self.service.invoke(method, v, out)
    }
    fn insert_child(&self, name: &str, obj: Arc<dyn Object>) -> Result<(), UnispaceError> {
        self.inner.insert_child(name, obj)
    }
    fn remove_child(&self, name: &str) -> Result<bool, UnispaceError> {
        self.inner.remove_child(name)
    }
}
