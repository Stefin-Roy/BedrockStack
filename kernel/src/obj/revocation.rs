//! P5 gate — cascade revocation + deny-list revocation proofs (§3.7, §8.6).
//!
//! Runs once from `Kernel::run()` after the mounts, on a **fresh test domain**
//! (disjoint table, registered so the projection tool sees it). It proves the
//! P5 gate (§RootGraph _Phase P5_ "Gate"):
//!
//! 1. **Cascade** — `revoke_cascade` on a family root severs the whole
//!    subtree in one operation (R8, §3.7.2): the returned size is root+children,
//!    every descendant becomes deny-marked, and no handle in the subtree stays
//!    `Live` in any projection.
//! 2. **Deny-list** — `revoke_deny` on a `Revocable` node makes future PERMIT
//!    fail `Revoked` while the cap **slot still exists** (Zombie, R9, §3.7.3).
//!
//! Read-only except for the cascade/deny state it exercises; it never invokes a
//! hook body. Arch-neutral.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::cap_handle::{CapHandle, CapId, HandleState, RevocationPolicy};
use super::contract::{ContractId, HookSignature, ReplyTag};
use super::domain::{register_domain, Domain};
use super::hook::HookId;
use super::rights::{CapRights, ContractRights, Rights};
use super::store::object_store;
use super::surface::{SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply};
use crate::drivers::serial::SerialPort;

const TEST_CONTRACT_SURF: SurfaceDesc = SurfaceDesc {
    kind: "test:access",
    attrs: &[],
    events: &[],
};

const TEST_HOOK_PING: HookId = HookId::of("ping");
const TEST_HOOKS: &[HookSignature] = &[HookSignature {
    name: "ping",
    params: &[],
    reply: ReplyTag::Data(&[TypeTag::U64]),
}];

const TEST_CONTRACT: ContractId = ContractId::of("test:access", &TEST_CONTRACT_SURF, &TEST_HOOKS);

/// A minimal gate object: a real node the test mints, exposing the `test:access`
/// contract (so PERMIT reaches the deny-list probe) and an optional `Revocable`
/// policy (§3.7.3). `dispatch` is a stub — the gate exercises PERMIT, not hooks.
struct GateObj {
    id: ObjId,
    kind: &'static str,
    revocable: bool,
}

impl GateObj {
    fn new(id: ObjId, kind: &'static str, revocable: bool) -> Self {
        GateObj { id, kind, revocable }
    }
}

impl Obj for GateObj {
    fn obj_id(&self) -> ObjId {
        self.id
    }

    fn kind(&self) -> &'static str {
        self.kind
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        &[TEST_CONTRACT]
    }

    fn revocation(&self) -> RevocationPolicy {
        if self.revocable {
            RevocationPolicy::Revocable
        } else {
            RevocationPolicy::DropDeath
        }
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        _hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        Err(ObjError::NotSupported)
    }
}

/// The P5 gate. Builds a test domain with a family root + 3 children, cascade-
/// revokes the root, asserts the whole subtree is deactivated; then deny-list
/// revokes a `Revocable` node and asserts PERMIT goes `Revoked` (Zombie).
pub fn run_p5_gate() {
    let store = object_store();
    let test: &'static Domain = Box::leak(Box::new(Domain::new(99)));
    register_domain(test);

    // ── 1. Cascade revocation (R8, §3.7.2) ──────────────────────────────
    // A drop-death family root carrying REVOKE, and three children registered
    // under it via the store's weak parent edges (so seal_cascade's §8.6 layer-1
    // walk reaches them).
    let root_id = store.next_id();
    let root: Arc<dyn Obj> = Arc::new(GateObj::new(root_id, "test:root", false));
    store.register_with_id_weak(root_id, root.kind(), None, None, &root);
    let root_cap = test.table.insert(CapHandle {
        id: CapId(0),
        node: root.clone(),
        rights: CapRights::new(Rights::INVOKE.or(Rights::REVOKE), ContractRights::empty()),
        state: HandleState::Live,
    });

    let mut children: Vec<Arc<dyn Obj>> = Vec::new();
    for _ in 0..3 {
        let cid = store.next_id();
        let c: Arc<dyn Obj> = Arc::new(GateObj::new(cid, "test:child", false));
        store.register_with_id_weak(cid, c.kind(), Some(root_id), Some(root_id), &c);
        children.push(c);
    }

    // Pre-state: the root cap is live in the test table, children reachable.
    assert!(
        test.table.snapshot().iter().any(|(_, n, _, _)| *n == root_id),
        "p5 gate: root cap not live before cascade"
    );

    // revoke_cascade returns root + children (subtree size).
    let severed = match test.table.revoke_cascade(root_cap) {
        Ok(n) => n,
        Err(e) => panic!("p5 gate: revoke_cascade failed: {:?}", e),
    };
    assert_eq!(severed, 4, "p5 gate: cascade must sever root + 3 children");

    // Whole subtree deny-marked (deactivated at next PERMIT, §8.6 layer 2) and
    // no handle stays Live in any projection.
    assert!(
        store.is_denied(root_id),
        "p5 gate: cascade root not deny-marked"
    );
    for c in &children {
        let id = c.obj_id();
        assert!(store.is_denied(id), "p5 gate: descendant 0x{:x} not severed", id.0);
    }
    assert!(
        !test.table.snapshot().iter().any(|(_, n, _, _)| *n == root_id),
        "p5 gate: root handle still live after cascade"
    );
    SerialPort::puts("[obj] p5 cascade: revoked 4-node subtree, all deactivated\n");

    // ── 2. Deny-list revocation (R9, §3.7.3) ───────────────────────────
    // A Revocable node: PERMIT passes until revoke_deny; after it fails Revoked
    // while the cap slot still exists (Zombie).
    let zid = store.next_id();
    let z: Arc<dyn Obj> = Arc::new(GateObj::new(zid, "test:revoked", true));
    store.register_with_id_weak(zid, z.kind(), None, None, &z);
    let zcap = test.table.insert(CapHandle {
        id: CapId(0),
        node: z.clone(),
        rights: CapRights::new(Rights::INVOKE, ContractRights::empty()),
        state: HandleState::Live,
    });

    match test.table.resolve(zcap, TEST_CONTRACT, TEST_HOOK_PING) {
        Ok(_) => {}
        Err(e) => panic!("p5 gate: revocable node refused before deny: {:?}", e),
    }

    store.revoke_deny(zid);
    match test.table.resolve(zcap, TEST_CONTRACT, TEST_HOOK_PING) {
        Err(ObjError::Revoked) => {}
        Ok(_) => panic!("p5 gate: deny-list PERMIT passed after revoke_deny"),
        Err(e) => panic!("p5 gate: deny-list produced wrong error {:?}", e),
    }
    assert!(
        test.table.snapshot().iter().any(|(_, n, _, s)| *n == zid && *s == HandleState::Live),
        "p5 gate: zombie cap must still exist (slot retained)"
    );
    SerialPort::puts("[obj] p5 deny-list: revoked node -> Zombie (cap retained, PERMIT Revoked)\n");

    SerialPort::puts("[obj] p5 gate: OK (cascade + deny-list)\n");
}

/// P5 census helper: point count of `test:*` records still in the store — the
/// "what died with that root" forensic residue (§8.8), printable before/after.
pub fn test_node_count() -> usize {
    let guard = object_store().lock_records();
    guard
        .iter()
        .filter(|(_, r)| r.kind == "test:root" || r.kind == "test:child" || r.kind == "test:revoked")
        .count()
}