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
/// P1 placeholder: the node is a `StubNode` whose dispatch refuses everything;
/// real heap/phys nodes arrive in P3. The `id` is assigned when the handle is
/// inserted into a table.
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

/// Minimal P1 node standing in for real service nodes.
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
        _hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        Err(ObjError::NotSupported)
    }
}