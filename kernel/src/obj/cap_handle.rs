use alloc::sync::Arc;

use super::rights::CapRights;
use super::Obj;

/// A capability identifier: an unforgeable table slot index (§3.2, §8.16).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CapId(pub u64);

/// Liveness state of a capability handle (§3.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleState {
    Live,
    Revoked,
    Zombie,
}

/// Whether an object is pure drop-death or deny-list revocable (§3.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevocationPolicy {
    DropDeath,
    Revocable,
}

/// A capability: `(id, node, rights, state)` (§3.2). `node` is a strong
/// reference, so `lifetime == reachability`.
#[derive(Clone)]
pub struct CapHandle {
    pub id: CapId,
    pub node: Arc<dyn Obj>,
    pub rights: CapRights,
    pub state: HandleState,
}