use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::filesystems::vfs::irq::IrqMutex;

use super::cap_handle::{CapHandle, CapId, HandleState, RevocationPolicy};
use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::{CapRights, ContractRights, Rights};
use super::store::object_store;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply, Value};

struct TableInner {
    slots: Vec<Option<CapHandle>>,
    free_list: Vec<u32>,
}

/// A domain's capability table, generalized from `vfs::fdtable` (§7.4).
///
/// `CapId` is the slot index into `slots`; the free-list reuses freed slots,
/// exactly like `FdTable` (see §8.16 on stale-id safety). The table lock is
/// an `IrqMutex`, so an ISR may safely re-enter the table (§8.13).
pub struct CapabilityTable {
    slots: IrqMutex<TableInner>,
}

impl CapabilityTable {
    pub const fn new() -> Self {
        CapabilityTable {
            slots: IrqMutex::new(TableInner {
                slots: Vec::new(),
                free_list: Vec::new(),
            }),
        }
    }

    /// Insert a capability into a fresh slot, returning its new `CapId`.
    pub fn insert(&self, handle: CapHandle) -> CapId {
        let mut inner = self.slots.lock();
        let idx = if let Some(i) = inner.free_list.pop() {
            i
        } else {
            let i = inner.slots.len() as u32;
            inner.slots.push(None);
            i
        };
        inner.slots[idx as usize] = Some(handle);
        CapId(idx as u64)
    }

    /// Insert a capability into a fresh slot, returning its new `CapId`.
    pub fn insert_handle(&self, h: CapHandle) -> CapId {
        self.insert(h)
    }

    /// Drop every handle, releasing all node references (§7.4). Used when a
    /// task's domain is torn down so its delegated capabilities stop keeping
    /// objects alive. The slot store itself is retained (the table stays
    /// allocated for the kernel lifetime).
    pub fn clear(&self) {
        let mut inner = self.slots.lock();
        inner.slots.clear();
        inner.free_list.clear();
    }

    /// Raw fetch without PERMIT (used by an object's own dispatch, which has
    /// already passed PERMIT; §7.4 item 3).
    pub fn get(&self, id: CapId) -> Result<Arc<dyn Obj>, ObjError> {
        let inner = self.slots.lock();
        inner
            .slots
            .get(id.0 as usize)
            .and_then(|s| s.as_ref())
            .map(|h| Arc::clone(&h.node))
            .ok_or(ObjError::NoSuchCap)
    }

    /// The PERMIT fast path (§7.5): slot fetch, Live, INVOKE, contract
    /// membership, the per-hook contract-right bit-test (§3.3), then the
    /// revocable deny-list probe (§3.3, §3.7.3). Returns the node's `Arc`
    /// *and* the invoking handle's [`CapRights`], copied under the same lock,
    /// so a downstream provider may check the exact rights the caller held
    /// (S1).
    ///
    /// Slope (one `IrqMutex` acquire, one array fetch, three bit-tests, one
    /// hash-set probe, no allocation — I8/§9.4):
    /// 1. slot fetch → `NoSuchCap`
    /// 2. `state != Live` → `Revoked`
    /// 3. universal `INVOKE` not held → `Denied`
    /// 4. node does not implement `contract` → `Denied`
    /// 5. handle's contract mask lacks the hook's required contract-right
    ///    (from `Obj::hook_contract_right`); an `empty()` mask is the
    ///    transitional "not yet narrowed" state and satisfies any requirement
    ///    (see `ContractRights` type docs).
    /// 6. `node.revocation() == Revocable` and the store's deny-list marks the
    ///    node → `Revoked` (a family-root cascade or a `revoke_deny` fired; the
    ///    node is a zombie — present, counted, inert, §3.7.3). A `HashSet`
    ///    probe is O(1), so the fast-path bound is preserved.
    ///
    /// The lock is released before the returned `Arc` and the copied rights
    /// are used.
    pub fn resolve_with_rights(
        &self,
        id: CapId,
        contract: ContractId,
        hook: HookId,
    ) -> Result<(Arc<dyn Obj>, CapRights), ObjError> {
        let inner = self.slots.lock();
        let h = inner
            .slots
            .get(id.0 as usize)
            .and_then(|s| s.as_ref())
            .ok_or(ObjError::NoSuchCap)?;
        if h.state != HandleState::Live {
            return Err(ObjError::Revoked);
        }
        if !h.rights.uni.contains(Rights::INVOKE) {
            return Err(ObjError::Denied);
        }
        if !h.node.contracts().contains(&contract) {
            return Err(ObjError::Denied);
        }
        let held = h.rights.contract;
        let required = h.node.hook_contract_right(contract, hook);
        if held != ContractRights::empty() && !held.contains(required) {
            return Err(ObjError::Denied);
        }
        if h.node.revocation() == RevocationPolicy::Revocable
            && object_store().is_denied(h.node.obj_id())
        {
            return Err(ObjError::Revoked);
        }
        // Copy the handle's rights while the lock is held (CapRights is Copy).
        Ok((Arc::clone(&h.node), h.rights))
    }

    /// Thin wrapper over [`resolve_with_rights`]: the PERMIT-only node fetch
    /// (§7.5), dropping the handle's rights.
    pub fn resolve(
        &self,
        id: CapId,
        contract: ContractId,
        hook: HookId,
    ) -> Result<Arc<dyn Obj>, ObjError> {
        self.resolve_with_rights(id, contract, hook)
            .map(|(node, _)| node)
    }

    /// Node fetch gated by the universal `QUERY` right only — the surface-read
    /// path (§4.1). Skips the INVOKE and contract-membership tests of
    /// [`resolve_with_rights`] (a surface is not a contract hook), but still
    /// requires the handle to be `Live` and still probes the revocable
    /// deny-list, so a revoked node's surface is inert too.
    pub fn resolve_for_query(&self, id: CapId) -> Result<(Arc<dyn Obj>, CapRights), ObjError> {
        let inner = self.slots.lock();
        let h = inner
            .slots
            .get(id.0 as usize)
            .and_then(|s| s.as_ref())
            .ok_or(ObjError::NoSuchCap)?;
        if h.state != HandleState::Live {
            return Err(ObjError::Revoked);
        }
        if !h.rights.uni.contains(Rights::QUERY) {
            return Err(ObjError::Denied);
        }
        if h.node.revocation() == RevocationPolicy::Revocable
            && object_store().is_denied(h.node.obj_id())
        {
            return Err(ObjError::Revoked);
        }
        Ok((Arc::clone(&h.node), h.rights))
    }

    /// Find the first Live cap in this table that resolves `contract`+`hook`
    /// under PERMIT (§2.7 graph composition). O(n) — tables are tiny (boot ~9,
    /// driver 4). Returns the CapId so the caller can `invoke` through it.
    pub fn resolve_first(&self, contract: ContractId, hook: HookId) -> Option<CapId> {
        // Snapshot the occupied slot ids under one lock, then run the PERMIT
        // fast path per candidate (resolve re-locks; IrqMutex is not
        // reentrant, so we never call it while holding `slots`).
        let ids: Vec<CapId> = {
            let inner = self.slots.lock();
            inner
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.is_some())
                .map(|(i, _)| CapId(i as u64))
                .collect()
        };
        for id in ids {
            if self.resolve(id, contract, hook).is_ok() {
                return Some(id);
            }
        }
        None
    }

    /// Duplicate a handle into a fresh slot with identical rights (§7.4).
    pub fn dup(&self, old: CapId) -> Result<CapId, ObjError> {
        let h = {
            let inner = self.slots.lock();
            inner
                .slots
                .get(old.0 as usize)
                .and_then(|s| s.as_ref())
                .ok_or(ObjError::NoSuchCap)?
                .clone()
        };
        Ok(self.insert(h))
    }

    /// Duplicate a handle into a fresh slot attuned to a subset of its rights
    /// (§7.4 item 2). Rights are monotone: the copy can never gain a bit.
    pub fn dup_limited(
        &self,
        old: CapId,
        keep: Rights,
        keep_contract: ContractRights,
    ) -> Result<CapId, ObjError> {
        let h = {
            let inner = self.slots.lock();
            inner
                .slots
                .get(old.0 as usize)
                .and_then(|s| s.as_ref())
                .ok_or(ObjError::NoSuchCap)?
                .clone()
        };
        let mut nh = h;
        nh.rights = nh.rights.attune(keep, keep_contract)?;
        nh.state = HandleState::Live;
        Ok(self.insert(nh))
    }

    /// Delegate a handle to another domain's table (§3.4.3, §8.24): a rights-
    /// preserving clone inserted into `target` under a fresh `CapId`. The source
    /// handle is untouched; delegation never amplifies.
    pub fn delegate(&self, target: &CapabilityTable, id: CapId) -> Result<CapId, ObjError> {
        let h = {
            let inner = self.slots.lock();
            inner
                .slots
                .get(id.0 as usize)
                .and_then(|s| s.as_ref())
                .ok_or(ObjError::NoSuchCap)?
                .clone()
        };
        Ok(target.insert(h))
    }

    /// Total allocated slots (occupied + freed-but-reserved), i.e. the table's
    /// high-water capacity. Read-only.
    pub fn capacity(&self) -> usize {
        self.slots.lock().slots.len()
    }

    /// Mark a handle `Revoked` (§3.7). The slot and its strong reference are
    /// retained; `resolve` fails with `ObjError::Revoked` from then on.
    pub fn revoke(&self, id: CapId) -> Result<(), ObjError> {
        let mut inner = self.slots.lock();
        match inner.slots.get_mut(id.0 as usize) {
            Some(Some(h)) => {
                h.state = HandleState::Revoked;
                Ok(())
            }
            _ => Err(ObjError::NoSuchCap),
        }
    }

    /// Cascade revocation of a family subtree (R8, §3.7.2, §9.2). Requires
    /// `REVOKE` in the handle's universal rights, else `Denied`. Marks the
    /// handle `Revoked`, seals the node's family root in the store (which
    /// deny-lists the root and every descendant, deactivating them lazily at
    /// the next PERMIT — §8.6 layer 2), and releases *this* table's strong ref
    /// to the root cap (the slot is freed; descendant refs in other tables are
    /// inert-but-held until dropped, §3.7.3). Returns the sealed subtree size
    /// for the §8.6 latency measurement.
    pub fn revoke_cascade(&self, id: CapId) -> Result<usize, ObjError> {
        let node = {
            let mut inner = self.slots.lock();
            let Some(h) = inner
                .slots
                .get_mut(id.0 as usize)
                .and_then(|s| s.as_mut())
            else {
                return Err(ObjError::NoSuchCap);
            };
            if !h.rights.uni.contains(Rights::REVOKE) {
                return Err(ObjError::Denied);
            }
            h.state = HandleState::Revoked;
            let node = Arc::clone(&h.node);
            // R8: release this table's strong ref to the subtree's root cap.
            inner.slots[id.0 as usize] = None;
            inner.free_list.push(id.0 as u32);
            node
        };
        Ok(object_store().seal_cascade(node.obj_id()))
    }

    /// Number of occupied slots. Separation proofs use it to assert a domain
    /// holds exactly what it was endowed with (§8.14, C8).
    pub fn count(&self) -> usize {
        let inner = self.slots.lock();
        inner.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Snapshot every occupied slot: `(CapId, node id, rights, state)`.
    /// Read-only; the lock is dropped before returning. The projection tool
    /// uses it for the `held-by` report and the leak detector for its
    /// reachability walk (§7.13, §8.7).
    pub fn snapshot(&self) -> Vec<(CapId, ObjId, CapRights, HandleState)> {
        let inner = self.slots.lock();
        inner
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.as_ref()
                    .map(|h| (CapId(i as u64), h.node.obj_id(), h.rights, h.state))
            })
            .collect()
    }
}

// ── The table as a node (§2.4, §7.8: "infrastructure is also nodes") ──────

/// The table node's own contract: introspection + administration hooks.
pub const TABLE_CONTRACT: ContractId = ContractId::of("infra:table", &TABLE_SURFACE, &TABLE_HOOKS);

pub const TABLE_COUNT: HookId = HookId::of("count");
pub const TABLE_SNAPSHOT_SIZE: HookId = HookId::of("snapshot_size");
pub const TABLE_REVOKE_CASCADE: HookId = HookId::of("revoke_cascade");
pub const TABLE_DELEGATE: HookId = HookId::of("delegate");

pub const TABLE_DOC: &str = "if you count(), you get the number of occupied slots; \
snapshot_size() reports high-water capacity; revoke_cascade(id) severs a family \
subtree; delegate(id, target) copies a handle into another domain's table.";

const TABLE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "infra:table",
    attrs: &[SurfaceAttr { name: "slots", ty: TypeTag::U64 }],
    events: &[],
};

const TABLE_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "count",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "snapshot_size",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "revoke_cascade",
        params: &[TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "delegate",
        params: &[TypeTag::U64, TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
];

static TABLE_CONTRACTS: &[ContractId] = &[TABLE_CONTRACT];

static TABLE_CONTRACT_DEF: Contract = Contract {
    id: TABLE_CONTRACT,
    name: "infra:table",
    surface: &TABLE_SURFACE,
    hooks: TABLE_HOOKS,
    doc: TABLE_DOC,
};

/// The canonical definition of the infra:table contract (§7.8).
pub fn table_contract_def() -> &'static Contract {
    &TABLE_CONTRACT_DEF
}

/// Stable identity for a table node (§7.8).
const TABLE_OBJ_ID: ObjId = ObjId(0x10_0009);

/// A thin `Obj` node adapter wrapping a [`CapabilityTable`] reference (§2.4,
/// §7.8). Exposes the table's introspection hooks (`count`, `snapshot_size`)
/// and its administration hooks (`revoke_cascade`, `delegate`) as
/// capability-gated operations, so a table is reachable and administrable as
/// a node. `revoke_cascade` requires the caller's handle to hold the universal
/// `REVOKE` right; delegation requires no amplification — the inserted copy
/// carries exactly the source's rights.
pub struct TableNode {
    table: &'static CapabilityTable,
}

impl TableNode {
    pub const fn new(table: &'static CapabilityTable) -> Self {
        TableNode { table }
    }

    /// The wrapped table (for clients that also need raw table operations).
    pub fn table(&self) -> &'static CapabilityTable {
        self.table
    }
}

impl Obj for TableNode {
    fn obj_id(&self) -> ObjId {
        TABLE_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "infra:table"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&TABLE_SURFACE)
    }

    fn surface_value<'a>(&self, name: &str) -> Option<Value<'a>> {
        match name {
            "slots" => Some(Value::U64(self.table.count() as u64)),
            _ => None,
        }
    }

    fn contracts(&self) -> &'static [ContractId] {
        TABLE_CONTRACTS
    }

    fn dispatch<'a>(
        &self,
        _caller: &CapabilityTable,
        rights: &CapRights,
        hook: HookId,
        args: &Args<'a>,
    ) -> Result<Reply<'a>, ObjError> {
        if hook == TABLE_COUNT {
            return Ok(Reply::Data(vec![Value::U64(self.table.count() as u64)]));
        }
        if hook == TABLE_SNAPSHOT_SIZE {
            return Ok(Reply::Data(vec![Value::U64(
                self.table.capacity() as u64
            )]));
        }
        if hook == TABLE_REVOKE_CASCADE {
            if !rights.uni.contains(Rights::REVOKE) {
                return Err(ObjError::Denied);
            }
            let id = match args.vals.first() {
                Some(Value::U64(id)) => CapId(*id),
                _ => return Err(ObjError::Denied),
            };
            let n = self.table.revoke_cascade(id)?;
            return Ok(Reply::Data(vec![Value::U64(n as u64)]));
        }
        if hook == TABLE_DELEGATE {
            let (target_id, cap_id) = match args.vals.first() {
                Some(Value::U64(t)) => match args.vals.get(1) {
                    Some(Value::U64(c)) => (*t, *c),
                    _ => return Err(ObjError::Denied),
                },
                _ => return Err(ObjError::Denied),
            };
            let target = super::domain::find_domain(target_id as u32).ok_or(ObjError::NoSuchCap)?;
            let new_id = self.table.delegate(&target.table, CapId(cap_id))?;
            return Ok(Reply::Data(vec![Value::U64(new_id.0)]));
        }
        Err(ObjError::NotSupported)
    }
}

/// The table node wrapping a table, for endowing a domain (§7.8).
pub fn table_node(table: &'static CapabilityTable) -> Arc<dyn Obj> {
    Arc::new(TableNode::new(table))
}