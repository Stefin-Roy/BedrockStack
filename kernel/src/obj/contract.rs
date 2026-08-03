use alloc::collections::BTreeMap;
use spin::Mutex;
use spin::Once;

use super::surface::{SurfaceDesc, TypeTag};
use super::ObjError;

/// A hook's reply schema (§7.2.4): data tuples, capability grants, or nothing.
#[derive(Clone, Copy, Debug)]
pub enum ReplyTag {
    Data(&'static [TypeTag]),
    Caps,
    None,
}

impl ReplyTag {
    /// Stable byte encoding for content-addressing (§7.2.4). For `Data`, the
    /// contained tags are hashed by the caller via [`ReplyTag::tags`].
    pub const fn discriminant(&self) -> u8 {
        match self {
            ReplyTag::Data(_) => 1,
            ReplyTag::Caps => 2,
            ReplyTag::None => 0,
        }
    }

    /// The data tuple for `Data` replies; empty for `Caps`/`None`.
    pub const fn tags(&self) -> &'static [TypeTag] {
        match self {
            ReplyTag::Data(t) => t,
            _ => &[],
        }
    }
}

/// A hook's full signature: name plus param/result schema (§7.2.4). The
/// signature is part of the hook's identity — two hooks with the same name but
/// different signatures are different hooks (§4.2).
#[derive(Clone, Copy, Debug)]
pub struct HookSignature {
    pub name: &'static str,
    pub params: &'static [TypeTag],
    pub reply: ReplyTag,
}

/// Content-addressed contract identity (§4.3, §7.2.4). The id is the FNV-1a
/// hash of the identity tuple `(name, surface schema, ordered hook
/// signatures)`. Renaming any part changes the identity — a breaking change.
/// Registry lookup is a P2 concern.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContractId(pub u64);

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
const SEP: u8 = 0xFF;

const fn fnv_byte(h: u64, b: u8) -> u64 {
    (h ^ b as u64).wrapping_mul(FNV_PRIME)
}

const fn fnv_bytes(mut h: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        h = fnv_byte(h, bytes[i]);
        i += 1;
    }
    h
}

impl ContractId {
    /// P2 content-addressing (§7.2.4). `const` so providers can expose stable
    /// contract identifiers at compile time.
    ///
    /// The byte stream is: `name` SEP `surface.kind` SEP then per-attr
    /// `(attr.name SEP attr.ty)` SEP, then per-hook
    /// `(hook.name SEP param.ty* SEP reply.ty reply.tag*)` SEP. The `0xFF`
    /// separators cannot appear in names (ASCII) nor in the small type
    /// discriminants, so every distinct tuple yields a distinct stream — hence
    /// distinct tuples never share an id (§I10).
    pub const fn of(
        name: &'static str,
        surface: &'static SurfaceDesc,
        hooks: &'static [HookSignature],
    ) -> ContractId {
        let mut h = FNV_OFFSET;

        h = fnv_bytes(h, name.as_bytes());
        h = fnv_byte(h, SEP);
        h = fnv_bytes(h, surface.kind.as_bytes());
        h = fnv_byte(h, SEP);

        let mut ai = 0;
        while ai < surface.attrs.len() {
            h = fnv_bytes(h, surface.attrs[ai].name.as_bytes());
            h = fnv_byte(h, SEP);
            h = fnv_byte(h, surface.attrs[ai].ty.discriminant());
            h = fnv_byte(h, SEP);
            ai += 1;
        }
        h = fnv_byte(h, SEP);

        let mut hi = 0;
        while hi < hooks.len() {
            h = fnv_bytes(h, hooks[hi].name.as_bytes());
            h = fnv_byte(h, SEP);
            let mut pi = 0;
            while pi < hooks[hi].params.len() {
                h = fnv_byte(h, hooks[hi].params[pi].discriminant());
                pi += 1;
            }
            h = fnv_byte(h, SEP);
            let reply = hooks[hi].reply;
            h = fnv_byte(h, reply.discriminant());
            let tags = reply.tags();
            let mut ti = 0;
            while ti < tags.len() {
                h = fnv_byte(h, tags[ti].discriminant());
                ti += 1;
            }
            h = fnv_byte(h, SEP);
            hi += 1;
        }

        ContractId(h)
    }
}

/// A contract/path: the promise a node makes about its hooks (§4.3, §7.2.4).
#[derive(Clone, Copy, Debug)]
pub struct Contract {
    pub id: ContractId,
    pub name: &'static str,
    pub surface: &'static SurfaceDesc,
    pub hooks: &'static [HookSignature],
    pub doc: &'static str,
}

/// The contract registry (§7.8): a node whose interior is the set of
/// registered contracts. It is where the content-addressed identity is
/// *validated* — two nodes claiming the same `ContractId` must hash to the
/// same tuple, or registration fails loudly (invariant **I10**): a
/// duplicate-name-with-different-signature bug surfaces here, not in the
/// field. Registering the *same* tuple twice is idempotent.
pub struct ContractRegistry {
    by_id: Mutex<BTreeMap<u64, &'static Contract>>,
}

impl ContractRegistry {
    pub const fn new() -> Self {
        ContractRegistry {
            by_id: Mutex::new(BTreeMap::new()),
        }
    }

    /// Register a contract, validating content-addressed identity (§7.8).
    ///
    /// * New id → insert.
    /// * Same id, same identity tuple → idempotent `Ok` (the tuple hashes to
    ///   the id, so a re-registration is the same contract).
    /// * Same id, different tuple → `ObjError::ContractCollision` (I10). The
    ///   stored entry is left untouched, so the registry never serves a
    ///   contract under an identity it did not hash to.
    pub fn register(&self, c: &'static Contract) -> Result<(), ObjError> {
        let mut map = self.by_id.lock();
        match map.get(&c.id.0) {
            Some(existing) => {
                if same_identity(existing, c) {
                    Ok(())
                } else {
                    Err(ObjError::ContractCollision)
                }
            }
            None => {
                map.insert(c.id.0, c);
                Ok(())
            }
        }
    }

    /// Resolve a contract id to its registered definition (§7.8).
    pub fn lookup(&self, id: ContractId) -> Option<&'static Contract> {
        self.by_id.lock().get(&id.0).copied()
    }

    /// Number of registered contracts. Forensics/observability only; the
    /// registry is not a namespace (§2.8).
    pub fn count(&self) -> usize {
        self.by_id.lock().len()
    }
}

/// Whether two contracts claim the same identity tuple — exactly the bytes
/// `ContractId::of` hashes (§7.2.4): `name`, `surface.kind`, each
/// `(attr.name, attr.ty)`, then each `(hook.name, param.ty*,
/// reply.tag, reply.ty*)`. Two distinct tuples therefore compare unequal, so
/// a collision means a caller handed the registry a `Contract` whose `id`
/// does not match its own content (the duplicate-name bug I10 catches).
fn same_identity(a: &Contract, b: &Contract) -> bool {
    if a.name != b.name || a.surface.kind != b.surface.kind {
        return false;
    }
    let (sa, sb) = (a.surface.attrs, b.surface.attrs);
    if sa.len() != sb.len() {
        return false;
    }
    for i in 0..sa.len() {
        if sa[i].name != sb[i].name || sa[i].ty.discriminant() != sb[i].ty.discriminant() {
            return false;
        }
    }
    let (ha, hb) = (a.hooks, b.hooks);
    if ha.len() != hb.len() {
        return false;
    }
    for i in 0..ha.len() {
        if ha[i].name != hb[i].name {
            return false;
        }
        let (pa, pb) = (ha[i].params, hb[i].params);
        if pa.len() != pb.len() {
            return false;
        }
        for j in 0..pa.len() {
            if pa[j].discriminant() != pb[j].discriminant() {
                return false;
            }
        }
        if ha[i].reply.discriminant() != hb[i].reply.discriminant() {
            return false;
        }
        let (ta, tb) = (ha[i].reply.tags(), hb[i].reply.tags());
        if ta.len() != tb.len() {
            return false;
        }
        for j in 0..ta.len() {
            if ta[j].discriminant() != tb[j].discriminant() {
                return false;
            }
        }
    }
    true
}

static CONTRACT_REGISTRY: Once<ContractRegistry> = Once::new();

/// Access the process-global contract registry, initializing it on first use.
///
/// Safe once the heap is up (all P2 users); the registry itself is a
/// const-constructible struct whose `BTreeMap` is empty until first insert.
pub fn contract_registry() -> &'static ContractRegistry {
    CONTRACT_REGISTRY.call_once(ContractRegistry::new)
}