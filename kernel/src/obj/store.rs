//! The object store: weak registry + id issuance (§7.3, §2.8). Not a namespace.
//!
//! Holds only weak references (I7) plus the cascade/deny kernel-side state
//! (§3.7.2, §3.7.3, §8.6). The store is consulted by the projection tool, the
//! cascade machinery, and the debugger — never as an access key.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hashbrown::{HashMap, HashSet};
use spin::Mutex;
use spin::Once;

use super::contract::{ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::CapRights;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply, Value};

/// A weak bookkeeping record of a node (§7.3). `weak` never keeps the node
/// alive (I7); the record is projection material, cascade bookkeeping, and
/// history only.
pub struct ObjRecord {
    pub id: ObjId,
    pub kind: String,
    pub parent: Option<ObjId>,
    pub family_root: Option<ObjId>,
    pub weak: Weak<dyn Obj>,
}

/// The object store: weak registry + id issuance (§7.3, §2.8). Not a namespace.
///
/// `cascade` is the per-root cascade state (§8.6 layer 1): keyed by family-root
/// id, `true` once that root has been sealed. `deny` is the per-object
/// deny-list (§3.7.3, R9) that a sealed root populates for its whole family,
/// deactivating descendants lazily at their next PERMIT (§8.6 layer 2).
pub struct ObjectStore {
    records: Mutex<BTreeMap<u64, ObjRecord>>,
    next_id: AtomicU64,
    cascade: Mutex<HashMap<u64, bool>>,
    deny: Mutex<HashSet<u64>>,
}

impl ObjectStore {
    // hashbrown's constructors are not const, so unlike `records` this cannot
    // be a `const fn`; the only caller is `Once::call_once` (runtime).
    pub fn new() -> Self {
        ObjectStore {
            records: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            cascade: Mutex::new(HashMap::new()),
            deny: Mutex::new(HashSet::new()),
        }
    }

    pub fn next_id(&self) -> ObjId {
        ObjId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Weak-register a node under a freshly-issued id (§7.3). Compatibility
    /// wrapper: no family membership and a dead `Weak` — the revocation-gate
    /// callers that
    /// know the node pass it via [`ObjectStore::register_weak`].
    pub fn register(&self, kind: &str, parent: Option<ObjId>) -> ObjId {
        let id = self.next_id();
        self.insert_record(id, kind, parent, None, dead_weak());
        id
    }

    /// Weak-register a node under a freshly-issued id with real family
    /// membership and a live `Weak` to the node (§7.3, §3.7.2). The store
    /// stays weak either way — `Weak::downgrade` holds no strong reference.
    pub fn register_weak(
        &self,
        kind: &str,
        parent: Option<ObjId>,
        family_root: Option<ObjId>,
        node: &Arc<dyn Obj>,
    ) -> ObjId {
        let id = self.next_id();
        self.insert_record(id, kind, parent, family_root, Arc::downgrade(node));
        id
    }

    /// Register a node under a *stable* id rather than a freshly-issued one
    /// (§7.3, §7.8). Infrastructure and adapter nodes carry deterministic
    /// `ObjId`s (e.g. `0x10_0000…0x10_0012`) so they are addressable for
    /// forensics before any counter bumps. Compatibility wrapper: no family
    /// membership and a dead `Weak`.
    pub fn register_with_id(&self, id: ObjId, kind: &str, parent: Option<ObjId>) {
        self.insert_record(id, kind, parent, None, dead_weak());
    }

    /// Register a node under a *stable* id with real family membership and a
    /// live `Weak` to the node (§7.3, §7.8, §3.7.2).
    pub fn register_with_id_weak(
        &self,
        id: ObjId,
        kind: &str,
        parent: Option<ObjId>,
        family_root: Option<ObjId>,
        node: &Arc<dyn Obj>,
    ) {
        self.insert_record(id, kind, parent, family_root, Arc::downgrade(node));
    }

    fn insert_record(
        &self,
        id: ObjId,
        kind: &str,
        parent: Option<ObjId>,
        family_root: Option<ObjId>,
        weak: Weak<dyn Obj>,
    ) {
        self.records
            .lock()
            .insert(id.0, ObjRecord { id, kind: String::from(kind), parent, family_root, weak });
    }

    /// Read-only access to the records for the projection tool (§2.8, §7.13).
    /// The store is not a namespace; this is forensics material only.
    pub fn lock_records(&self) -> spin::MutexGuard<'_, BTreeMap<u64, ObjRecord>> {
        self.records.lock()
    }

    /// Seal a family (§3.7.2, §8.6). `root_id` may name any node in the
    /// family: the record's `family_root` edge resolves it to the true root
    /// (a root node, `family_root = None`, seals under its own id). Sets the
    /// per-root cascade state (§8.6 layer 1) and deny-lists the root plus every
    /// descendant following `family_root`/`parent` edges — deactivating the
    /// whole family lazily at the next PERMIT (§8.6 layer 2). No strong ref is
    /// released here; [`super::table::CapabilityTable::revoke_cascade`] does
    /// that. Returns the number of nodes marked by this call (the subtree size
    /// on first seal; `0` on a re-seal of an already-sealed root) for the §8.6
    /// latency measurement.
    pub fn seal_cascade(&self, root_id: ObjId) -> usize {
        let root = {
            let records = self.records.lock();
            records
                .get(&root_id.0)
                .and_then(|r| r.family_root)
                .unwrap_or(root_id)
        };
        self.cascade.lock().insert(root.0, true);

        let mut deny = self.deny.lock();
        let mut severed = if deny.insert(root.0) { 1 } else { 0 };

        // Build a children adjacency over family_root/parent edges, then BFS
        // from the root. Idempotent: a node already denied is not re-counted.
        let children: HashMap<u64, Vec<u64>> = {
            let records = self.records.lock();
            let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
            for (&id, rec) in records.iter() {
                if let Some(p) = rec.parent {
                    children.entry(p.0).or_default().push(id);
                }
                if let Some(fr) = rec.family_root {
                    children.entry(fr.0).or_default().push(id);
                }
            }
            children
        };
        let mut frontier = vec![root.0];
        while let Some(cur) = frontier.pop() {
            for &child in children.get(&cur).into_iter().flatten() {
                if deny.insert(child) {
                    severed += 1;
                    frontier.push(child);
                }
            }
        }
        severed
    }

    /// Whether the family rooted at `root_id` has been cascade-sealed (§8.6
    /// layer 1, per-root cascade state).
    pub fn is_cascade_severed(&self, root_id: ObjId) -> bool {
        self.cascade.lock().get(&root_id.0).copied().unwrap_or(false)
    }

    /// Set the per-object deny-list flag (R9, §3.7.3): a `Revocable` node's
    /// PERMIT fails from now on, even while caps still hold strong refs.
    pub fn revoke_deny(&self, node_id: ObjId) {
        self.deny.lock().insert(node_id.0);
    }

    /// Read the per-object deny-list flag (§3.7.3, §7.5). O(1) hash probe —
    /// the deny bit-test of `PERMIT` (I8, step 6).
    pub fn is_denied(&self, node_id: ObjId) -> bool {
        self.deny.lock().contains(&node_id.0)
    }
}

/// A dead `Weak<dyn Obj>` for records that never knew their node (§7.3).
/// This nightly's `Weak::new` is Sized-only, so build a dead `Weak<StoreNode>`
/// (StoreNode is Sized and `Unsize<dyn Obj>`) and unsize-coerce; the result
/// upgrades to `None` forever and never keeps a node alive (I7).
fn dead_weak() -> Weak<dyn Obj> {
    Weak::<StoreNode>::new()
}

static OBJECT_STORE: Once<ObjectStore> = Once::new();

/// Access the process-global object store, initializing it on first use.
///
/// Safe once the heap is up (all Seed-phase users); the store itself is a
/// const-constructible struct whose `BTreeMap` is empty until first insert.
pub fn object_store() -> &'static ObjectStore {
    OBJECT_STORE.call_once(ObjectStore::new)
}

// ── The store as a node (§2.4, §7.8: "infrastructure is also nodes") ──────

/// The store node's own contract: weak-registry introspection (§7.3, §7.8).
pub const STORE_CONTRACT: ContractId = ContractId::of("infra:store", &STORE_SURFACE, &STORE_HOOKS);

pub const STORE_COUNT: HookId = HookId::of("count");
pub const STORE_LOOKUP: HookId = HookId::of("lookup");
pub const STORE_DENIED: HookId = HookId::of("denied");

const STORE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "infra:store",
    attrs: &[SurfaceAttr { name: "records", ty: TypeTag::U64 }],
    events: &[],
};

const STORE_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "count",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "lookup",
        params: &[TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64, TypeTag::U64]),
    },
    HookSignature {
        name: "denied",
        params: &[TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
];

static STORE_CONTRACTS: &[ContractId] = &[STORE_CONTRACT];

/// Stable identity for the store node (§7.8).
const STORE_OBJ_ID: ObjId = ObjId(0x10_0011);

/// A thin `Obj` node adapter over the [`ObjectStore`] singleton (§2.4, §7.8).
/// Exposes the store's weak registry as read-only forensics hooks (`count`,
/// `lookup`, `denied`) — the store is consulted by the projection tool and the
/// cascade machinery, never as an access key (§2.8).
pub struct StoreNode;

impl Obj for StoreNode {
    fn obj_id(&self) -> ObjId {
        STORE_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "infra:store"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&STORE_SURFACE)
    }

    fn surface_value(&self, name: &str) -> Option<Value> {
        match name {
            "records" => Some(Value::U64(object_store().lock_records().len() as u64)),
            _ => None,
        }
    }

    fn contracts(&self) -> &'static [ContractId] {
        STORE_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        let store = object_store();
        if hook == STORE_COUNT {
            return Ok(Reply::Data(vec![Value::U64(
                store.lock_records().len() as u64
            )]));
        }
        if hook == STORE_LOOKUP {
            let id = match args.vals.first() {
                Some(Value::U64(id)) => *id,
                _ => return Err(ObjError::Denied),
            };
            let records = store.lock_records();
            let rec = records.get(&id).ok_or(ObjError::NoSuchCap)?;
            return Ok(Reply::Data(vec![
                Value::U64(rec.parent.map(|p| p.0).unwrap_or(0)),
                Value::U64(rec.family_root.map(|f| f.0).unwrap_or(0)),
                Value::U64(store.is_denied(rec.id) as u64),
            ]));
        }
        if hook == STORE_DENIED {
            let id = match args.vals.first() {
                Some(Value::U64(id)) => *id,
                _ => return Err(ObjError::Denied),
            };
            return Ok(Reply::Data(vec![Value::U64(
                store.is_denied(ObjId(id)) as u64
            )]));
        }
        Err(ObjError::NotSupported)
    }
}

/// The store node, for endowing a domain (§7.8).
pub fn store_node() -> Arc<dyn Obj> {
    Arc::new(StoreNode)
}
