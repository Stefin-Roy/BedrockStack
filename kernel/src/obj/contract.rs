use super::surface::SurfaceDesc;

/// Content-addressed contract identity (§4.3). Registry lookup is a P2 concern.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContractId(pub u64);

impl ContractId {
    /// P1 stub: FNV-1a over the name. Full content-addressing is P2 (§7.2.4).
    /// `const` so providers can expose stable contract identifiers.
    pub const fn from_name(s: &'static str) -> ContractId {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let bytes = s.as_bytes();
        let mut h = OFFSET;
        let mut i = 0;
        while i < bytes.len() {
            h ^= bytes[i] as u64;
            h = h.wrapping_mul(PRIME);
            i += 1;
        }
        ContractId(h)
    }
}

/// A hook's name; the param/reply schema is deferred (§7.2.4).
pub struct HookSignature {
    pub name: &'static str,
}

/// A contract/path: the promise a node makes about its hooks (§4.3).
pub struct Contract {
    pub id: ContractId,
    pub name: &'static str,
    pub surface: Option<&'static SurfaceDesc>,
}