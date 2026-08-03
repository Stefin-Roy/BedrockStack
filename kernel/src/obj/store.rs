use alloc::collections::BTreeMap;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use spin::Once;

use super::ObjId;

/// A weak bookkeeping record of a node (§7.3). Holds no reference to the node.
pub struct ObjRecord {
    pub id: ObjId,
    pub kind: String,
    pub parent: Option<ObjId>,
}

/// The object store: weak registry + id issuance (§7.3, §2.8). Not a namespace.
pub struct ObjectStore {
    records: Mutex<BTreeMap<u64, ObjRecord>>,
    next_id: AtomicU64,
}

impl ObjectStore {
    pub const fn new() -> Self {
        ObjectStore {
            records: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn next_id(&self) -> ObjId {
        ObjId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    pub fn register(&self, kind: &str, parent: Option<ObjId>) -> ObjId {
        let id = self.next_id();
        self.records
            .lock()
            .insert(id.0, ObjRecord { id, kind: String::from(kind), parent });
        id
    }

    /// Read-only access to the records for the projection tool (§2.8, §7.13).
    /// The store is not a namespace; this is forensics material only.
    pub fn lock_records(&self) -> spin::MutexGuard<'_, BTreeMap<u64, ObjRecord>> {
        self.records.lock()
    }
}

static OBJECT_STORE: Once<ObjectStore> = Once::new();

/// Access the process-global object store, initializing it on first use.
///
/// Safe once the heap is up (all P1 users); the store itself is a
/// const-constructible struct whose `BTreeMap` is empty until first insert.
pub fn object_store() -> &'static ObjectStore {
    OBJECT_STORE.call_once(ObjectStore::new)
}