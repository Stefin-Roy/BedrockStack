use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::rights::{CapRights, ContractRights, Rights};
use super::store::object_store;
use super::{Obj, ObjError};

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

/// Mint a first capability over an already-constructed real node (§7.6, §7.10).
/// Callable ONLY by the Principal (acting through the rooted bootstrapper,
/// before self-revoke).
///
/// The physical-world nodes (`PhysMemNode`, `HeapNode`, …) carry their own
/// stable family-root `ObjId` (e.g. `0x11_0000`), which is registered in the
/// store under its stable id and kind. No fresh id is allocated — the node
/// already owns its identity. `first_contract` seeds the handle's contract-right
/// mask (READ/WRITE/CALL), so the physical roots carry real masks from the first
/// commit and the per-hook gate is live from boot.
pub fn mint_node(
    _caller: &PrincipalContext,
    node: Arc<dyn Obj>,
    first_rights: Rights,
    first_contract: ContractRights,
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
        rights: CapRights::new(first_rights, first_contract),
        state: HandleState::Live,
    })
}