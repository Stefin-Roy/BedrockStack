//! RootGraph object-graph / capability model (§ Document/RootGraph.md).
//!
//! Everything here is arch-neutral (§8.15): per-CPU state is reached through
//! the `crate::smp` wrappers, never through arch-specific code.

extern crate alloc;

use alloc::vec::Vec;

pub mod adapters;
pub mod bootstrap;
pub mod cap_handle;
pub mod clients;
pub mod contract;
pub mod domain;
pub mod driver;
pub mod hook;
pub mod mint;
pub mod registry;
pub mod rights;
pub mod separation;
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
use hook::HookId;

/// Globally unique node identity. Confers nothing; used by the store and by
/// forensics only. Never an access key (§2.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjId(pub u64);

/// The node. Every object in the system implements this (§7.2.1).
pub trait Obj: Send + Sync {
    fn obj_id(&self) -> ObjId;

    fn kind(&self) -> &'static str;

    fn surface(&self) -> Option<&'static SurfaceDesc>;

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

    /// Active face (§4.2). `caller` is the invoking domain's table, threaded
    /// per §6.3.
    fn dispatch(
        &self,
        caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError>;
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
pub enum Reply {
    None,
    Data(Vec<Value>),
    Caps(Vec<CapHandle>),
}

/// A hook argument value: a scalar, a label, or a buffer.
pub enum Value {
    U64(u64),
    Str(&'static str),
    Buf(Vec<u8>),
}

/// Hook arguments.
pub struct Args {
    pub vals: Vec<Value>,
}

impl Args {
    pub fn none() -> Self {
        Args { vals: Vec::new() }
    }
}

/// The single dispatch entry point (§7.9): PERMIT via `resolve`, then the
/// node's hook body. Capabilities in the reply are inserted into the caller's
/// table (getting real `CapId`s) before the reply is returned.
pub fn invoke(
    table: &CapabilityTable,
    id: CapId,
    contract: ContractId,
    hook: HookId,
    args: &Args,
) -> Result<Reply, ObjError> {
    let node = table.resolve(id, contract, hook)?;
    match node.dispatch(table, hook, args)? {
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