use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystems::vfs::irq::IrqMutex;

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract::{ContractId, HookSignature};
use super::hook::HookId;
use super::rights::{ContractRights, Rights};
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply};

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
    /// membership, then the per-hook contract-right bit-test (§3.3).
    ///
    /// Slope (one `IrqMutex` acquire, one array fetch, three bit-tests, no
    /// allocation — I8/§9.4):
    /// 1. slot fetch → `NoSuchCap`
    /// 2. `state != Live` → `Revoked`
    /// 3. universal `INVOKE` not held → `Denied`
    /// 4. node does not implement `contract` → `Denied`
    /// 5. handle's contract mask lacks the hook's required contract-right
    ///    (from `Obj::hook_contract_right`); an `empty()` mask is the
    ///    transitional "not yet narrowed" state and satisfies any requirement
    ///    (see `ContractRights` type docs).
    ///
    /// The revocable deny-list check (§3.3: `node.revocation()==Revocable` and
    /// `store.deny(node)`) is reserved for P3, when revocable nodes arrive; it
    /// will slot in after step 5 and fail with `Revoked`.
    ///
    /// The lock is released before the returned `Arc` is used.
    pub fn resolve(
        &self,
        id: CapId,
        contract: ContractId,
        hook: HookId,
    ) -> Result<Arc<dyn Obj>, ObjError> {
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
        Ok(Arc::clone(&h.node))
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

    /// Number of occupied slots. Separation proofs use it to assert a domain
    /// holds exactly what it was endowed with (§8.14, C8).
    pub fn count(&self) -> usize {
        let inner = self.slots.lock();
        inner.slots.iter().filter(|s| s.is_some()).count()
    }
}

// ── The table as a node (§2.4, §7.8: "infrastructure is also nodes") ──────

/// The table node's own contract: identity + surface only in this phase.
pub const TABLE_CONTRACT: ContractId = ContractId::of("infra:table", &TABLE_SURFACE, &TABLE_HOOKS);

const TABLE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "infra:table",
    attrs: &[SurfaceAttr { name: "slots", ty: TypeTag::U64 }],
    events: &[],
};

const TABLE_HOOKS: &[HookSignature] = &[];

static TABLE_CONTRACTS: &[ContractId] = &[TABLE_CONTRACT];

/// Stable identity for a table node (§7.8).
const TABLE_OBJ_ID: ObjId = ObjId(0x10_0012);

/// A thin `Obj` node adapter wrapping a [`CapabilityTable`] reference (§2.4,
/// §7.8). This phase exposes identity + surface only; `dispatch` is a stub so
/// the table is a capability-reachable object without rearchitecting it.
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

    fn contracts(&self) -> &'static [ContractId] {
        TABLE_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        _hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        Err(ObjError::NotSupported)
    }
}

/// The table node wrapping a table, for endowing a domain (§7.8).
pub fn table_node(table: &'static CapabilityTable) -> Arc<dyn Obj> {
    Arc::new(TableNode::new(table))
}