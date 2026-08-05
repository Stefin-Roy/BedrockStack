use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract::ContractId;
use super::hook::HookId;
use super::rights::{CapRights, ContractRights, Rights};
use super::store::object_store;
use super::surface::SurfaceDesc;
use super::table::CapabilityTable;
use super::{Args, Obj, ObjError, ObjId, Reply};

/// The single-use seed context under which minting is permitted (§7.6).
/// Opaque: only the bootstrap seed can enter it; after `run()` begins it is
/// no longer enterable.
pub struct PrincipalContext;

/// Frozen once the bootstrapper self-revokes; any later `mint` fails (§8.2).
static MINT_FROZEN: AtomicBool = AtomicBool::new(false);

/// Freeze the mint guard after the boot domain's endowment is finalized (§8.2).
pub fn finalize_mint() {
    MINT_FROZEN.store(true, Ordering::Relaxed);
}

/// Create a new family root and its first capability (§7.6). Callable ONLY by
/// the Principal (acting through the rooted bootstrapper, before self-revoke).
///
/// Seed placeholder: the node is a `StubNode` whose dispatch refuses everything;
/// the `id` is assigned when the handle is inserted into a table. Superseded
/// for primitives by [`mint_node`] (PhysicalNodes phase), which mints over a real node; `mint`
/// remains for ad-hoc stub roots until they are switched over.
pub fn mint(
    _caller: &PrincipalContext,
    kind: &'static str,
    first_rights: Rights,
) -> Result<CapHandle, ObjError> {
    // §8.2 authorized path: once frozen, a well-behaved caller gets
    // `MintAuthorityGone` rather than a panic.
    if MINT_FROZEN.load(Ordering::Relaxed) {
        return Err(ObjError::MintAuthorityGone);
    }
    // §8.2 development canary: the guard must not have been frozen between the
    // check above and this point. A concurrent `finalize_mint()` on another CPU
    // (or a re-entrant ISR-path mint) is exactly the race this turns loud.
    assert!(
        !MINT_FROZEN.load(Ordering::Relaxed),
        "mint after endowment finalization"
    );
    let id = object_store().register(kind, None);
    let node: Arc<dyn Obj> = Arc::new(StubNode { id });
    Ok(CapHandle {
        id: CapId(0),
        node,
        rights: CapRights::new(first_rights, ContractRights::empty()),
        state: HandleState::Live,
    })
}

/// Mint a first capability over an already-constructed real node (§7.6, §7.10).
/// Callable ONLY by the Principal (acting through the rooted bootstrapper,
/// before self-revoke).
///
/// Unlike [`mint`] it does not build a `StubNode`: the physical-world nodes
/// (`PhysMemNode`, `HeapNode`, …) carry their own stable family-root `ObjId`
/// (e.g. `0x11_0000`), which is registered in the store under its stable id and
/// kind. No fresh id is allocated — the node already owns its identity.
pub fn mint_node(
    _caller: &PrincipalContext,
    node: Arc<dyn Obj>,
    first_rights: Rights,
) -> Result<CapHandle, ObjError> {
    // §8.2 authorized path: once frozen, a well-behaved caller gets
    // `MintAuthorityGone` rather than a panic.
    if MINT_FROZEN.load(Ordering::Relaxed) {
        return Err(ObjError::MintAuthorityGone);
    }
    // §8.2 development canary: the guard must not have been frozen between the
    // check above and this point. A concurrent `finalize_mint()` on another CPU
    // (or a re-entrant ISR-path mint) is exactly the race this turns loud.
    assert!(
        !MINT_FROZEN.load(Ordering::Relaxed),
        "mint_node after endowment finalization"
    );
    object_store().register_with_id(node.obj_id(), node.kind(), None);
    Ok(CapHandle {
        id: CapId(0),
        node,
        rights: CapRights::new(first_rights, ContractRights::empty()),
        state: HandleState::Live,
    })
}

/// Minimal Seed-phase node standing in for real service nodes.
struct StubNode {
    id: ObjId,
}

impl Obj for StubNode {
    fn obj_id(&self) -> ObjId {
        self.id
    }

    fn kind(&self) -> &'static str {
        "stub:placeholder"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        &[]
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        _rights: &CapRights,
        _hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        Err(ObjError::NotSupported)
    }
}