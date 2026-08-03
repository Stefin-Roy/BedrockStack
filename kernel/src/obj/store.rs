use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use spin::Once;

use super::contract::{ContractId, HookSignature};
use super::hook::HookId;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply};

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

    /// Register a node under a *stable* id rather than a freshly-issued one
    /// (§7.3, §7.8). Infrastructure and adapter nodes carry deterministic
    /// `ObjId`s (e.g. `0x10_0000…0x10_0012`) so they are addressable for
    /// forensics before any counter bumps. The store stays weak either way —
    /// the record holds no reference to the node. Used by `bootstrap()` so the
    /// `kerneldump graph` census can see infra/adapters that were never minted.
    pub fn register_with_id(&self, id: ObjId, kind: &str, parent: Option<ObjId>) {
        self.records
            .lock()
            .insert(id.0, ObjRecord { id, kind: String::from(kind), parent });
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

// ── The store as a node (§2.4, §7.8: "infrastructure is also nodes") ──────

/// The store node's own contract: identity + surface only in this phase; its
/// hooks arrive with the rest of P3's node wiring.
pub const STORE_CONTRACT: ContractId = ContractId::of("infra:store", &STORE_SURFACE, &STORE_HOOKS);

const STORE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "infra:store",
    attrs: &[SurfaceAttr { name: "records", ty: TypeTag::U64 }],
    events: &[],
};

const STORE_HOOKS: &[HookSignature] = &[];

static STORE_CONTRACTS: &[ContractId] = &[STORE_CONTRACT];

/// Stable identity for the store node (§7.8).
const STORE_OBJ_ID: ObjId = ObjId(0x10_0011);

/// A thin `Obj` node adapter over the [`ObjectStore`] singleton (§2.4, §7.8).
/// This phase exposes identity + surface only; `dispatch` is a stub so the
/// node is a capability-reachable object without rearchitecting the store.
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

    fn contracts(&self) -> &'static [ContractId] {
        STORE_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        Err(ObjError::NotSupported)
    }
}

/// The store node, for endowing a domain (§7.8).
pub fn store_node() -> Arc<dyn Obj> {
    Arc::new(StoreNode)
}