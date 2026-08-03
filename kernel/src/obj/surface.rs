/// A surface is a node's passive, typed, read-only data face (§4.1).
///
/// The schema is part of the contract's _identity_ (§7.2.5): renaming the
/// `kind` or any attribute labels/types changes the `ContractId` (§7.2.4).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceDesc {
    pub kind: &'static str,
    pub attrs: &'static [SurfaceAttr],
    pub events: &'static [EventDesc],
}

/// A tagged attribute value type for surface reads (§7.2.5).
#[derive(Clone, Copy, Debug)]
pub enum TypeTag {
    U64,
    Str,
    Buf,
}

impl TypeTag {
    /// Stable byte encoding used in content-addressing. Kept distinct from
    /// the `0xFF` field separators so contract byte-streams stay decodable.
    pub const fn discriminant(&self) -> u8 {
        match self {
            TypeTag::U64 => 0,
            TypeTag::Str => 1,
            TypeTag::Buf => 2,
        }
    }
}

/// A typed surface attribute (a label, e.g. "size", "model") (§7.2.5).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceAttr {
    pub name: &'static str,
    pub ty: TypeTag,
}

/// An optional event-stream descriptor (§7.2.5). Event names/types are also
/// part of the contract identity.
#[derive(Clone, Copy, Debug)]
pub struct EventDesc {
    pub name: &'static str,
    pub ty: TypeTag,
}