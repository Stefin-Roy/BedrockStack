//! The Contract Registry as a node (§7.8).
//!
//! The registry is infrastructure, and infrastructure is also nodes (§2.4):
//! holding a registry capability is how a driver queries "what does
//! `block:storage` promise?" — it is discovery by owned capability, never
//! ambient. The registry's own hooks (`register`, `lookup`) require `INVOKE`;
//! only domains endowed with the registry cap can consult it.
//!
//! The node adapter wraps the process-global functional [`ContractRegistry`]
//! (`super::contract::contract_registry()`); the interior lives there. Arch-neutral.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;

use super::adapters;
use super::contract::{self, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::surface::{SurfaceDesc, TypeTag};
use super::table::CapabilityTable;
use super::{Args, Obj, ObjError, ObjId, Reply, Value};

/// The registry's own contract: the promise a registry node makes.
pub const REGISTRY_CONTRACT: ContractId =
    ContractId::of("infra:registry", &REGISTRY_SURFACE, &REGISTRY_HOOKS);
pub const REGISTRY_REGISTER: HookId = HookId::of("register");
pub const REGISTRY_LOOKUP: HookId = HookId::of("lookup");

const REGISTRY_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "infra:registry",
    attrs: &[],
    events: &[],
};

const REGISTRY_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "register",
        params: &[TypeTag::Str],
        reply: ReplyTag::None,
    },
    HookSignature {
        name: "lookup",
        params: &[TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::Str, TypeTag::Str]),
    },
];

static REGISTRY_CONTRACTS: &[ContractId] = &[REGISTRY_CONTRACT];

/// Stable identity for the registry node (§7.8).
const REGISTRY_OBJ_ID: ObjId = ObjId(0x10_0010);

/// The node adapter over the process-global [`ContractRegistry`]. A unit
/// struct: the interior lives in `contract_registry()`, reached through
/// [`Obj::dispatch`] only after PERMIT has checked the registry capability.
pub struct RegistryNode;

impl Obj for RegistryNode {
    fn obj_id(&self) -> ObjId {
        REGISTRY_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "infra:registry"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&REGISTRY_SURFACE)
    }

    fn contracts(&self) -> &'static [ContractId] {
        REGISTRY_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        let registry = contract::contract_registry();
        if hook == REGISTRY_REGISTER {
            let name = match args.vals.first() {
                Some(Value::Str(name)) => *name,
                _ => return Err(ObjError::Denied),
            };
            let def = adapters::contract_def(name).ok_or(ObjError::NotSupported)?;
            registry.register(def)?;
            return Ok(Reply::None);
        }
        if hook == REGISTRY_LOOKUP {
            let id = match args.vals.first() {
                Some(Value::U64(id)) => ContractId(*id),
                _ => return Err(ObjError::Denied),
            };
            return match registry.lookup(id) {
                Some(c) => Ok(Reply::Data(vec![Value::Str(c.name), Value::Str(c.doc)])),
                None => Ok(Reply::None),
            };
        }
        Err(ObjError::NotSupported)
    }
}

/// The registry node, for endowing a domain (§7.8). `Arc<dyn Obj>` so it drops
/// into a `CapHandle` like any other provider node.
pub fn registry_node() -> Arc<dyn Obj> {
    Arc::new(RegistryNode)
}