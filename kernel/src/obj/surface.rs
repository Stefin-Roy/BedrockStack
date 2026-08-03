/// A surface is a node's passive, typed, read-only data face (§4.1).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceDesc {
    pub kind: &'static str,
}

/// A tagged attribute value type for surface reads (§7.2.5).
#[derive(Clone, Copy, Debug)]
pub enum TypeTag {
    U64,
    Str,
    Buf,
}

/// A typed surface attribute (a label, e.g. "size", "model") (§7.2.5).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceAttr {
    pub name: &'static str,
    pub ty: TypeTag,
}