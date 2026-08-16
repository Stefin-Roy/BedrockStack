//! # Declared objects — kernel subsystems publish objects without a struct
//!
//! Traditional unispace providers hand-write a `struct` + `impl Object` per
//! object, with compile-time `&'static` schemas and method tables.  Declared
//! objects are the runtime counterpart: a subsystem builds an arbitrary object
//! from owned [`OwnedSchema`]s, owned method descriptors, and plain `fn`
//! handlers, then attaches it anywhere with [`super::connect`].  This is what
//! lets an in-kernel subsystem "declare whatever, whenever" — a hot-plugged
//! device, a service endpoint, or a whole subtree — without new types.
//!
//! ## Discipline
//!
//! - Handlers are plain `fn` pointers (like the input-layer `poll` hooks), so
//!   a declared object is `Send + Sync` with no closure captures.  Subsystems
//!   that need state keep it in their own globals/locks and close over nothing.
//! - Every handler returns `Result`; `panic = "abort"` means a handler that
//!   panics on request data is a kernel bug.  Decode/encode of request values
//!   against the declared schemas is done by the core *before* any handler
//!   runs, so handlers only see schema-valid `Value`s.
//! - Schema/method tables are owned and live as long as the object `Arc`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::dir::SimpleDir;
use super::schema::{OwnedSchema, Value};
use super::{ListingEntry, Object, ObjectKind, OwnedMethodDesc, UnispaceError};

/// A read handler for a declared object's value.
pub type ReadHandler = fn(out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError>;

/// A write handler for a declared object's value.  Receives the already
/// schema-decoded value.
pub type WriteHandler = fn(v: &Value) -> Result<(), UnispaceError>;

/// One owned method of a declared object: its descriptor plus the dispatch
/// handler.  The handler receives the schema-decoded input `Value` and appends
/// any output bytes to `out`.
pub struct OwnedMethod {
    pub desc: OwnedMethodDesc,
    pub handler: fn(&Value, &mut Vec<u8>) -> Result<(), UnispaceError>,
}

/// Builder for a [`DeclaredObject`].
pub struct Declare {
    kind: ObjectKind,
    schema: OwnedSchema,
    methods: Vec<OwnedMethod>,
    read: Option<ReadHandler>,
    write: Option<WriteHandler>,
    children: bool,
}

impl Declare {
    pub fn new(kind: ObjectKind) -> Self {
        Declare {
            kind,
            schema: OwnedSchema::Unit,
            methods: Vec::new(),
            read: None,
            write: None,
            children: false,
        }
    }

    /// The object's value schema (what `read(/path)` serializes, and what
    /// `write(/path, …)` validates against).
    pub fn value(mut self, schema: OwnedSchema) -> Self {
        self.schema = schema;
        self
    }

    /// Set the read handler.  Without one, a value read of an object that is
    /// not a directory returns `Unsupported`.
    pub fn read(mut self, h: ReadHandler) -> Self {
        self.read = Some(h);
        self
    }

    /// Set the write handler.  Without one, value writes return `Unsupported`.
    pub fn write(mut self, h: WriteHandler) -> Self {
        self.write = Some(h);
        self
    }

    /// Add an owned method (an *action* the object exposes via
    /// `write(/path:name, …)`).
    pub fn method(
        mut self,
        name: &str,
        input: OwnedSchema,
        output: OwnedSchema,
        handler: fn(&Value, &mut Vec<u8>) -> Result<(), UnispaceError>,
    ) -> Self {
        self.methods.push(OwnedMethod {
            desc: OwnedMethodDesc {
                name: String::from(name),
                input,
                output,
            },
            handler,
        });
        self
    }

    /// Give the declared object dynamic children — it becomes a directory that
    /// `connect` can attach objects into (and under), hot-plug style.
    pub fn children(mut self) -> Self {
        self.children = true;
        self
    }

    pub fn build(self) -> Arc<dyn Object> {
        let mut descs = Vec::with_capacity(self.methods.len());
        for m in &self.methods {
            descs.push(OwnedMethodDesc {
                name: m.desc.name.clone(),
                input: m.desc.input.clone(),
                output: m.desc.output.clone(),
            });
        }
        Arc::new(DeclaredObject {
            kind: self.kind,
            schema: self.schema,
            methods: self.methods,
            descs,
            read: self.read,
            write: self.write,
            children: if self.children {
                Some(SimpleDir::new())
            } else {
                None
            },
        })
    }
}

/// A runtime-built [`Object`]: owned schema, owned methods, `fn` handlers, and
/// optional dynamic children.  See the module docs.
pub struct DeclaredObject {
    kind: ObjectKind,
    schema: OwnedSchema,
    methods: Vec<OwnedMethod>,
    /// Mirrors `methods[*].desc` so `owned_methods()` can return borrowed
    /// descriptors without leaking.
    descs: Vec<OwnedMethodDesc>,
    read: Option<ReadHandler>,
    write: Option<WriteHandler>,
    children: Option<SimpleDir>,
}

impl Object for DeclaredObject {
    fn kind(&self) -> ObjectKind {
        self.kind
    }

    fn owned_value_schema(&self) -> Option<&OwnedSchema> {
        Some(&self.schema)
    }

    fn owned_methods(&self) -> &[OwnedMethodDesc] {
        &self.descs
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        self.children.as_ref().and_then(|d| d.resolve(name))
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        if let Some(d) = &self.children {
            d.list(out)?;
        }
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        if self.kind == ObjectKind::Dir {
            let mut entries = Vec::new();
            self.list(&mut entries)?;
            return super::encode_listing(entries, out);
        }
        match self.read {
            Some(h) => h(out, max),
            None => Err(UnispaceError::Unsupported),
        }
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        match self.write {
            Some(h) => h(&v),
            None => Err(UnispaceError::Unsupported),
        }
    }

    fn invoke(&self, method: usize, v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let m = self
            .methods
            .get(method)
            .ok_or(UnispaceError::MethodNotFound)?;
        (m.handler)(&v, out)
    }

    fn insert_child(&self, name: &str, obj: Arc<dyn Object>) -> Result<(), UnispaceError> {
        match &self.children {
            Some(d) => {
                d.insert(name, obj);
                Ok(())
            }
            None => Err(UnispaceError::Unsupported),
        }
    }

    fn remove_child(&self, name: &str) -> Result<bool, UnispaceError> {
        match &self.children {
            Some(d) => Ok(d.remove(name)),
            None => Err(UnispaceError::Unsupported),
        }
    }
}
