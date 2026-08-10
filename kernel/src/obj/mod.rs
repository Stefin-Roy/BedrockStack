//! RootGraph object-graph / capability model (§ Document/RootGraph.md).
//!
//! Everything here is arch-neutral (§8.15): per-CPU state is reached through
//! the `crate::smp` wrappers, never through arch-specific code.

extern crate alloc;

use alloc::vec::Vec;

pub mod adapters;
pub mod bootstrap;
pub mod devices;
pub mod cap_handle;
pub mod contract;
pub mod domain;
pub mod driver;
pub mod fs;
pub mod hook;
pub mod memregion;
pub mod mint;
pub mod nodes;
pub mod registry;
pub mod rights;
pub mod store;
pub mod surface;
pub mod table;

pub use cap_handle::{CapHandle, CapId, HandleState, RevocationPolicy};
pub use contract::{Contract, ContractId, ContractRegistry, HookSignature, ReplyTag};
pub use registry::{REGISTRY_CONTRACT, REGISTRY_LOOKUP, REGISTRY_REGISTER, RegistryNode};
pub use rights::{CapRights, ContractRights, Rights};
pub use store::StoreNode;
pub use surface::{EventDesc, SurfaceAttr, SurfaceDesc, TypeTag};
pub use table::TableNode;

use table::CapabilityTable;
use hook::{HookId, SURFACE_READ};

/// Globally unique node identity. Confers nothing; used by the store and by
/// forensics only. Never an access key (§2.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjId(pub u64);

/// The node. Every object in the system implements this (§7.2.1).
pub trait Obj: Send + Sync {
    fn obj_id(&self) -> ObjId;

    fn kind(&self) -> &'static str;

    fn surface(&self) -> Option<&'static SurfaceDesc>;

    /// Read one typed attribute off this node's surface by name (§4.1).
    /// Default `None`: the node exposes no dynamic value for that attribute,
    /// and the surface read answers `NotSupported`. Overridden by nodes that
    /// want live surface values reachable through a `QUERY`-bearing cap (the
    /// `SURFACE_READ` hook is handled centrally in `invoke`).
    fn surface_value<'a>(&self, _name: &str) -> Option<Value<'a>> {
        None
    }

    fn contracts(&self) -> &'static [ContractId];

    fn revocation(&self) -> RevocationPolicy {
        RevocationPolicy::DropDeath
    }

    /// The contract-right a caller must hold to invoke `hook` on this node
    /// under `contract` (§3.3). This feeds the third bit-test of `PERMIT`
    /// (§7.5): the fast path requires the handle's contract mask to contain
    /// the returned right (or be `empty()`, the transitional "not yet
    /// narrowed" mask — see `ContractRights`).
    ///
    /// The default is `CALL`, so providers that do not discriminate per-hook
    /// rights (today's adapters) inherit it unchanged. A future provider may
    /// override this to demand `READ`/`WRITE` for specific hooks, turning the
    /// third bit-test into a real per-hook read/write gate without touching
    /// the fast path.
    fn hook_contract_right(&self, contract: ContractId, hook: HookId) -> ContractRights {
        let _ = (contract, hook);
        ContractRights::CALL
    }

    /// Downcast-free access to an interrupt entry point (§7.10.5).
    ///
    /// Only nodes that *are* handlers — a [`super::nodes::IrqHandlerNode`],
    /// materialized by the kernel over a vetted `fn()` — return `Some`. This is
    /// what lets the `Irq` node's `register_handler` bind a handler from a
    /// capability instead of trusting a raw function address passed in the
    /// arguments: the address comes from a node the kernel materialized, never
    /// from a caller-supplied scalar.
    fn as_handler(&self) -> Option<fn()> {
        None
    }

    /// Downcast access to the concrete node (§7.x). Default `None`; a node that
    /// must share its concrete interior (e.g. `BlockNode` so a mount can
    /// recover the `BlockDevice` handle) overrides this to return `Some(self)`.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// Active face (§4.2). `caller` is the invoking domain's table, threaded
    /// per §6.3; `rights` is the invoking handle's [`CapRights`] as copied by
    /// [`CapabilityTable::resolve_with_rights`] under the same PERMIT that let
    /// this hook through (§7.5), so a provider may gate its hook body on the
    /// exact rights the caller held (S1). It is a *check* handle only: the
    /// invocation already passed PERMIT, and no amplification is possible
    /// through this reference.
    fn dispatch<'a>(
        &self,
        caller: &CapabilityTable,
        rights: &CapRights,
        hook: HookId,
        args: &Args<'a>,
    ) -> Result<Reply<'a>, ObjError>;
}

/// Errors returned by the capability machinery (§7.2, §8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjError {
    NoSuchCap,
    Denied,
    Revoked,
    Disowned,
    NoAmplification,
    OutOfMemory,
    MintAuthorityGone,
    Exhausted,
    NotSupported,
    ContractCollision,
}

impl core::fmt::Display for ObjError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            ObjError::NoSuchCap => "no such capability",
            ObjError::Denied => "denied",
            ObjError::Revoked => "revoked",
            ObjError::Disowned => "disowned",
            ObjError::NoAmplification => "no amplification",
            ObjError::OutOfMemory => "out of memory",
            ObjError::MintAuthorityGone => "mint authority gone",
            ObjError::Exhausted => "id space exhausted",
            ObjError::NotSupported => "operation not supported",
            ObjError::ContractCollision => "contract id collision: distinct signatures claim the same id",
        };
        f.write_str(s)
    }
}

/// A hook's reply: data, capabilities, or nothing (§7.9).
pub enum Reply<'a> {
    None,
    Data(Vec<Value<'a>>),
    Caps(Vec<CapHandle>),
}

/// A hook argument value: a scalar, a label, or a buffer.
pub enum Value<'a> {
    U64(u64),
    Str(&'a str),
    Buf(Vec<u8>),
}

/// Hook arguments.
pub struct Args<'a> {
    pub vals: Vec<Value<'a>>,
}

impl<'a> Args<'a> {
    pub fn none() -> Self {
        Args { vals: Vec::new() }
    }
}

/// The single dispatch entry point (§7.9): PERMIT via `resolve`, then the
/// node's hook body. Capabilities in the reply are inserted into the caller's
/// table (getting real `CapId`s) before the reply is returned.
pub fn invoke<'a>(
    table: &CapabilityTable,
    id: CapId,
    contract: ContractId,
    hook: HookId,
    args: &Args<'a>,
) -> Result<Reply<'a>, ObjError> {
    // §4.1 surface reads: node-level, gated by the universal `QUERY` right, and
    // exempt from contract membership — a surface is not a contract hook. The
    // attribute name is the sole argument; the value comes from
    // `Obj::surface_value`.
    if hook == SURFACE_READ {
        let (node, _) = table.resolve_for_query(id)?;
        let name = match args.vals.first() {
            Some(Value::Str(n)) => *n,
            _ => return Err(ObjError::Denied),
        };
        return match node.surface_value(name) {
            Some(v) => Ok(Reply::Data(alloc::vec![v])),
            None => Err(ObjError::NotSupported),
        };
    }
    let (node, rights) = table.resolve_with_rights(id, contract, hook)?;
    match node.dispatch(table, &rights, hook, args)? {
        Reply::Caps(caps) => {
            let mut inserted = Vec::new();
            for h in caps {
                let mut entry = h.clone();
                entry.id = table.insert_handle(h);
                inserted.push(entry);
            }
            Ok(Reply::Caps(inserted))
        }
        other => Ok(other),
    }
}