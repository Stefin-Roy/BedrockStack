/// A hook identifier — a hash of `(kind, op, signature)` (§4.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HookId(pub u64);

impl HookId {
    /// Simple FNV-1a over the identifier. Signature content-addressing is a Trinity-phase concern.
    /// `const` so providers can expose stable hook identifiers in `dispatch`.
    pub const fn of(name: &'static str) -> HookId {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let bytes = name.as_bytes();
        let mut h = OFFSET;
        let mut i = 0;
        while i < bytes.len() {
            h ^= bytes[i] as u64;
            h = h.wrapping_mul(PRIME);
            i += 1;
        }
        HookId(h)
    }
}