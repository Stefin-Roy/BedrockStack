use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystems::vfs::irq::IrqMutex;

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract::ContractId;
use super::rights::{ContractRights, Rights};
use super::{Obj, ObjError};

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
    /// membership. The lock is released before the returned `Arc` is used.
    pub fn resolve(&self, id: CapId, contract: ContractId) -> Result<Arc<dyn Obj>, ObjError> {
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
        Ok(Arc::clone(&h.node))
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