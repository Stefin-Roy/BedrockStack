use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::filesystems::vfs::irq::IrqMutex;

use super::schema::{MethodDesc, Schema, Value};
use super::{ListingEntry, Object, ObjectKind, UnispaceError, DIR_SCHEMA};

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
    }

    pub fn remove(&self, name: &str) -> bool {
        self.children.lock().remove(name).is_some()
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

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        let children = self.children.lock();
        let mut names: Vec<String> = children.keys().cloned().collect();
        names.sort();
        for n in names {
            let kind = children.get(&n).map(|o| o.kind()).unwrap_or(ObjectKind::Service);
            out.push(ListingEntry { name: n, kind });
        }
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut entries = Vec::new();
        self.list(&mut entries)?;
        super::encode_listing(entries, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::PermissionDenied)
    }

    fn invoke(&self, _method: usize, _v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        Err(UnispaceError::MethodNotFound)
    }
}
