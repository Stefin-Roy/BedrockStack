//! # Unispace — the Namespace Object System
//!
//! Unispace is the kernel's unified object namespace.  It exposes exactly two
//! primitives — Read and Write — over a single `/`-rooted tree.
//!
//! ## What unispace owns
//!
//! Unispace owns `/` and nothing else.  The root is a directory whose children
//! are the *systems* (VFS mounts, `/sys`, future `/dev`, ...).  `read(/)`
//! enumerates every registered system.  Below the first component, unispace
//! holds zero structure: every path step is a pure `resolve()` delegation into
//! provider-owned objects.  Providers own their trees; unispace only walks,
//! validates, and dispatches.
//!
//! ## The two primitives
//!
//! - `read(/a/b)` — the object's value, serialized per its schema.
//! - `read(/a/b:desc)` — the object's schema (kind + value schema + methods).
//! - `read(/a/b:method)` — that method's input/output schema.
//! - `write(/a/b, data)` — decode+validate against the value schema, apply.
//! - `write(/a/b:method, data)` — decode+validate against the method's input
//!   schema, dispatch, return any output.
//!
//! `:desc` is handled by unispace itself and exists on every object.  Folders
//! are objects too (`read(dir)` returns the listing), so `/A/folder:mkdir` is
//! legal.
//!
//! ## Optional flags (`arg4`)
//!
//! Every `read`/`write` call may carry a provider-defined `flags` word (the
//! syscall `r10` register). Unispace never interprets it: an object that does
//! not implement flag semantics rejects a nonzero value with `Unsupported`
//! (`-ENOSYS`) rather than ignoring it, so a flags-bearing intent on the wrong
//! object fails loudly. Providers choose the meaning — the VFS file object
//! uses it for read-at / append / write-at (see `provider/vfs.rs`). `0` always
//! means a plain value read/write.
//!
//! ## Discipline
//!
//! Every path, payload, and schema is validated with `Result` — malformed
//! input may only produce `Err`, never a panic (`panic = "abort"`).

pub mod decl;
pub mod dir;
pub mod path;
pub mod provider;
pub mod schema;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use spin::Once;

use crate::filesystems::vfs::error::VfsError;

pub use dir::SimpleDir;
pub use path::parse as parse_path;

use schema::{MethodDesc, OwnedSchema, Schema, Value};

/// A runtime-declared method descriptor, mirroring [`MethodDesc`] over owned
/// schemas.  Used by [`Object::owned_methods`] on dynamically declared objects
/// (see [`decl`]); providers with static tables return `&[]`.
#[derive(Debug, Clone)]
pub struct OwnedMethodDesc {
    pub name: String,
    pub input: OwnedSchema,
    pub output: OwnedSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    File,
    Dir,
    Device,
    Service,
}

impl ObjectKind {
    pub fn tag(self) -> u8 {
        match self {
            ObjectKind::File => 0,
            ObjectKind::Dir => 1,
            ObjectKind::Device => 2,
            ObjectKind::Service => 3,
        }
    }

    pub fn from_tag(t: u8) -> Option<ObjectKind> {
        match t {
            0 => Some(ObjectKind::File),
            1 => Some(ObjectKind::Dir),
            2 => Some(ObjectKind::Device),
            3 => Some(ObjectKind::Service),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::File => "file",
            ObjectKind::Dir => "dir",
            ObjectKind::Device => "device",
            ObjectKind::Service => "service",
        }
    }
}

#[derive(Debug)]
pub enum UnispaceError {
    InvalidPath,
    NotFound,
    NotADirectory,
    IsADirectory,
    MethodNotFound,
    DecodeError,
    SchemaMismatch,
    /// The requested operation does not exist for this object (a read-only
    /// leaf written without a `:method`, or a schema descriptor written at
    /// all).  Not an access-control result — permissions are not implemented.
    Unsupported,
    /// The backing store could not satisfy an allocation (maps to `-ENOMEM`).
    /// Carried by memory-managing methods (e.g. `/proc/self:mmap`).
    OutOfMemory,
    /// A request referenced an invalid address space (maps to `-EFAULT`).
    BadAddress,
    /// The request argument was structurally valid but semantically rejected
    /// (maps to `-EINVAL`, e.g. a non-page-aligned `munmap`).
    InvalidArgument,
    Vfs(VfsError),
}

impl From<VfsError> for UnispaceError {
    fn from(e: VfsError) -> Self {
        UnispaceError::Vfs(e)
    }
}

#[derive(Debug, Clone)]
pub struct ListingEntry {
    pub name: String,
    pub kind: ObjectKind,
}

/// The universal object interface.  Providers implement behavior; unispace
/// resolves paths through `resolve()` and dispatches through the rest.
///
/// An object is either *static* (compile-time `&'static Schema`/`[MethodDesc]`,
/// the traditional provider style) or *declared* (runtime `OwnedSchema` +
/// `[OwnedMethodDesc]`, built by [`decl::DeclaredObject`]).  Exactly one style
/// is meaningful per object; the owned forms take precedence when present.
pub trait Object: Send + Sync {
    fn kind(&self) -> ObjectKind;

    /// Compile-time value schema (default `Unit`; static providers override).
    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_UNIT
    }

    /// Runtime value schema, if this is a dynamically declared object.
    fn owned_value_schema(&self) -> Option<&OwnedSchema> {
        None
    }

    /// Compile-time method table (default empty; static providers override).
    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    /// Runtime method table, if this is a dynamically declared object.
    fn owned_methods(&self) -> &[OwnedMethodDesc] {
        &[]
    }

    /// Directory capability (leaf default: no children).
    fn resolve(&self, _name: &str) -> Option<Arc<dyn Object>> {
        None
    }

    fn list(&self, _out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError>;

    /// Read the object's value under a provider-defined `flags` word (the
    /// syscall `arg4`/`r10`). Unispace never interprets `flags`: the default
    /// rejects a nonzero value with `Unsupported` (`-ENOSYS`) so a flags
    /// bearing intent on an object that does not implement it fails loudly
    /// instead of silently behaving like a plain value read. Providers that
    /// want semantics (e.g. the VFS file object: read-at offset) override this.
    fn read_value_flags(
        &self,
        out: &mut Vec<u8>,
        max: usize,
        flags: u64,
    ) -> Result<(), UnispaceError> {
        if flags != 0 {
            Err(UnispaceError::Unsupported)
        } else {
            self.read_value(out, max)
        }
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }

    /// Write the object's value under a provider-defined `flags` word (the
    /// syscall `arg4`/`r10`). Same discipline as `read_value_flags`: a nonzero
    /// `flags` on an object without flag semantics returns `Unsupported`
    /// rather than being dropped (an ignored append bit would truncate a
    /// file). Providers override to interpret `flags`.
    fn write_value_flags(&self, v: Value, flags: u64) -> Result<(), UnispaceError> {
        if flags != 0 {
            Err(UnispaceError::Unsupported)
        } else {
            self.write_value(v)
        }
    }

    fn invoke(&self, _method: usize, _v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        Err(UnispaceError::MethodNotFound)
    }

    /// Directory mutation capability (default: immutable).  Only dynamic
    /// directories — the root, [`SimpleDir`], and [decl]'s declared dirs —
    /// override this; connect/disconnect refuse otherwise.
    fn insert_child(&self, _name: &str, _obj: Arc<dyn Object>) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }

    fn remove_child(&self, _name: &str) -> Result<bool, UnispaceError> {
        Err(UnispaceError::Unsupported)
    }
}

// ── Shared schema for directory listings ───────────────────────────────

pub(crate) static KIND_VARIANTS: [schema::EnumVariant; 4] = [
    schema::EnumVariant {
        name: "file",
        value: 0,
    },
    schema::EnumVariant {
        name: "dir",
        value: 1,
    },
    schema::EnumVariant {
        name: "device",
        value: 2,
    },
    schema::EnumVariant {
        name: "service",
        value: 3,
    },
];

static DIR_SCHEMA_ENTRY: Schema = Schema::Struct(&[
    schema::Field {
        name: "name",
        ty: &schema::SCHEMA_STR,
    },
    schema::Field {
        name: "kind",
        ty: &Schema::Enum(&KIND_VARIANTS),
    },
]);

static DIR_SCHEMA_LIST: Schema = Schema::List(&DIR_SCHEMA_ENTRY);

/// Wire schema for `read(dir)`: `struct{ entries: list<{name: str, kind: enum}> }`.
pub static DIR_SCHEMA: Schema = Schema::Struct(&[schema::Field {
    name: "entries",
    ty: &DIR_SCHEMA_LIST,
}]);

pub(crate) fn encode_listing(
    entries: Vec<ListingEntry>,
    out: &mut Vec<u8>,
) -> Result<(), UnispaceError> {
    // Direct wire encoding of `DIR_SCHEMA` (`struct{ entries: list<{name: str,
    // kind: enum}> }`) — no intermediate `Value` tree, so a listing costs one
    // string copy per entry instead of a per-entry `Value::Struct`/`Value::Str`
    // allocation plus a second recursive traversal.
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        crate::unispace::schema::write_len_string(out, &e.name);
        out.extend_from_slice(&(e.kind.tag() as u32).to_le_bytes());
    }
    Ok(())
}

// ── Root registry ──────────────────────────────────────────────────────

static ROOT: Once<Arc<SimpleDir>> = Once::new();

/// Create the `/` registry.  Must be called once, after the heap is live and
/// before any system registers itself.
pub fn init() {
    ROOT.call_once(|| Arc::new(SimpleDir::new()));
}

fn root() -> &'static Arc<SimpleDir> {
    ROOT.get().expect("unispace: init() not called")
}

/// Attach a system's root object at `/name`.
pub fn register(name: &str, obj: Arc<dyn Object>) -> Result<(), UnispaceError> {
    if name.is_empty() || name.contains('/') || name.contains(':') || name == "." || name == ".." {
        return Err(UnispaceError::InvalidPath);
    }
    root().insert(name, obj);
    Ok(())
}

/// Detach a system (e.g. a hot-plugged drive).
pub fn unregister(name: &str) -> bool {
    root().remove(name)
}

/// Connect a subsystem's object (or whole tree) at an arbitrary path such as
/// `/audio/dev0`.  Missing intermediate components are auto-created as
/// [`SimpleDir`]s **only** while the walk is still inside dynamic directories
/// (the root itself, or a `SimpleDir`/declared dir).  Walking into an immutable
/// provider tree (VFS `/A`, `/proc`, `/sys`, ...) and finding a missing
/// component returns `Unsupported` — the core never injects foreign children
/// into provider-owned trees.  The final component is inserted into its parent
/// (creating it on a dynamic parent if absent is not allowed; objects must be
/// pre-built with [`decl`] or similar).
///
/// A re-connect to a path that already exists **replaces** the entry (the old
/// `Arc` is dropped when the last resolver releases it).
pub fn connect(path: &str, obj: Arc<dyn Object>) -> Result<(), UnispaceError> {
    let parsed = path::parse(path)?;
    if parsed.components.is_empty() || parsed.method.is_some() {
        return Err(UnispaceError::InvalidPath);
    }
    let components = &parsed.components;

    // Walk to the parent of the final component, creating intermediate
    // SimpleDirs only through mutable dirs that permit insertion.
    let mut current: Arc<dyn Object> = root().clone();
    for comp in &components[..components.len() - 1] {
        match current.resolve(comp) {
            Some(child) => current = child,
            None => {
                let dir: Arc<dyn Object> = Arc::new(SimpleDir::new());
                current.insert_child(comp, dir.clone())?;
                current = dir;
            }
        }
    }
    let last = &components[components.len() - 1];
    current.insert_child(last, obj)
}

/// Disconnect a subsystem object at an arbitrary path.  Returns `true` if an
/// entry was removed, `false` if the path did not exist.  Intermediate path
/// components must already exist; a missing parent is `NotFound` (connect
/// auto-creates dirs, disconnect does not tear extras down).
pub fn disconnect(path: &str) -> Result<bool, UnispaceError> {
    let parsed = path::parse(path)?;
    if parsed.components.is_empty() || parsed.method.is_some() {
        return Err(UnispaceError::InvalidPath);
    }
    let components = &parsed.components;
    let mut current: Arc<dyn Object> = root().clone();
    for comp in &components[..components.len() - 1] {
        match current.resolve(comp) {
            Some(child) => current = child,
            None => return Err(UnispaceError::NotFound),
        }
    }
    let last = &components[components.len() - 1];
    current.remove_child(last)
}

// ── Resolution & dispatch ──────────────────────────────────────────────

/// Resolve a path to its target object and optional `:method` selector.
pub fn resolve(path: &str) -> Result<(Arc<dyn Object>, Option<&str>), UnispaceError> {
    let parsed = path::parse(path)?;
    let mut current: Arc<dyn Object> = root().clone();
    for comp in &parsed.components {
        if current.kind() != ObjectKind::Dir {
            return Err(UnispaceError::NotADirectory);
        }
        match current.resolve(comp) {
            Some(obj) => current = obj,
            None => return Err(UnispaceError::NotFound),
        }
    }
    Ok((current, parsed.method))
}

fn find_method(obj: &dyn Object, name: &str) -> Option<usize> {
    if !obj.owned_methods().is_empty() {
        obj.owned_methods().iter().position(|m| m.name == name)
    } else {
        obj.methods().iter().position(|m| m.name == name)
    }
}

/// Read an object's value, or (with `:method`/`:desc`) a schema.
///
/// `max` bounds how many bytes the object may emit into `out`, so the kernel
/// stops allocating once a caller's buffer is satisfied (a hostile `len` on
/// `sys_read` cannot force a full-object heap allocation). Object value reads
/// never exceed `max`; schema/desc reads are inherently small.
pub fn read(path: &str, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
    read_flags(path, out, max, 0)
}

/// Read with a provider-defined `flags` word. `flags == 0` is identical to
/// `read`; a nonzero value has semantics chosen by the target object (see the
/// module doc "Optional flags" section).
pub fn read_flags(
    path: &str,
    out: &mut Vec<u8>,
    max: usize,
    flags: u64,
) -> Result<(), UnispaceError> {
    let (obj, method) = resolve(path)?;
    match method {
        Some("desc") => {
            let mut v = Vec::new();
            encode_object_desc(&*obj, &mut v)?;
            out.extend_from_slice(&v[..core::cmp::min(max, v.len())]);
            Ok(())
        }
        Some(m) => {
            let idx = find_method(&*obj, m).ok_or(UnispaceError::MethodNotFound)?;
            let mut v = Vec::new();
            if !obj.owned_methods().is_empty() {
                encode_method_desc_owned(&obj.owned_methods()[idx], &mut v)?;
            } else {
                encode_method_desc(&obj.methods()[idx], &mut v)?;
            }
            out.extend_from_slice(&v[..core::cmp::min(max, v.len())]);
            Ok(())
        }
        None => {
            if obj.kind() == ObjectKind::Dir {
                if flags != 0 {
                    return Err(UnispaceError::Unsupported);
                }
                let mut entries = Vec::new();
                obj.list(&mut entries)?;
                encode_listing(entries, out)
            } else {
                obj.read_value_flags(out, max, flags)
            }
        }
    }
}

/// Write an object's value, or invoke a method.  Method output (if any) is
/// appended to `out`; value writes leave it untouched.
pub fn write(path: &str, data: &[u8], out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    write_flags(path, data, out, 0)
}

/// Write with a provider-defined `flags` word. `flags == 0` is identical to
/// `write`; a nonzero value has semantics chosen by the target object (e.g. a
/// file's append / write-at modes).
pub fn write_flags(
    path: &str,
    data: &[u8],
    out: &mut Vec<u8>,
    flags: u64,
) -> Result<(), UnispaceError> {
    let (obj, method) = resolve(path)?;
    match method {
        Some("desc") => Err(UnispaceError::Unsupported),
        Some(m) => {
            if flags != 0 {
                return Err(UnispaceError::Unsupported);
            }
            let idx = find_method(&*obj, m).ok_or(UnispaceError::MethodNotFound)?;
            let value = if !obj.owned_methods().is_empty() {
                schema::decode_value_owned(data, &obj.owned_methods()[idx].input)?
            } else {
                schema::decode_value(data, obj.methods()[idx].input)?
            };
            obj.invoke(idx, value, out)
        }
        None => {
            if obj.kind() == ObjectKind::Dir {
                return Err(UnispaceError::IsADirectory);
            }
            let value = match obj.owned_value_schema() {
                Some(s) => schema::decode_value_owned(data, s)?,
                None => schema::decode_value(data, obj.value_schema())?,
            };
            obj.write_value_flags(value, flags)
        }
    }
}

fn encode_object_desc(obj: &dyn Object, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    out.push(obj.kind().tag());
    match obj.owned_value_schema() {
        Some(s) => schema::encode_schema_owned(s, out),
        None => schema::encode_schema(obj.value_schema(), out),
    }
    if !obj.owned_methods().is_empty() {
        let methods = obj.owned_methods();
        out.extend_from_slice(&(methods.len() as u32).to_le_bytes());
        for md in methods {
            encode_method_desc_owned(md, out)?;
        }
    } else {
        let methods = obj.methods();
        out.extend_from_slice(&(methods.len() as u32).to_le_bytes());
        for md in methods {
            encode_method_desc(md, out)?;
        }
    }
    Ok(())
}

fn encode_method_desc(md: &MethodDesc, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    schema::write_len_string(out, md.name);
    schema::encode_schema(md.input, out);
    schema::encode_schema(md.output, out);
    Ok(())
}

fn encode_method_desc_owned(md: &OwnedMethodDesc, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    schema::write_len_string(out, &md.name);
    schema::encode_schema_owned(&md.input, out);
    schema::encode_schema_owned(&md.output, out);
    Ok(())
}

// ── Boot self-test ─────────────────────────────────────────────────────

/// Exercise read/write/schema dispatch over the registered providers and print
/// results to serial.  Non-fatal: every step logs its own outcome. Gated
/// behind the `selftest` feature.
#[cfg(feature = "selftest")]
pub fn self_test() {
    use crate::drivers::serial::SerialPort;
    SerialPort::puts("[unispace] self-test start\n");
    let mut out = Vec::new();

    // Listing of the root (the system registry).
    match read("/", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &DIR_SCHEMA) {
            Ok(v) => {
                SerialPort::puts("read(/) = ");
                SerialPort::puts(&schema::value_text(&v, &DIR_SCHEMA));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: decode / failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read / failed: {:?}", e),
    }

    // The /proc provider: process pseudo-FS. At self-test time no task exists
    // yet (runs before the scheduler smoke test), so the listing is empty and
    // /proc/self resolves to NotFound (no current task in kernel context).
    out.clear();
    match read("/proc", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &DIR_SCHEMA) {
            Ok(v) => {
                SerialPort::puts("read(/proc) = ");
                SerialPort::puts(&schema::value_text(&v, &DIR_SCHEMA));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: decode /proc failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /proc failed: {:?}", e),
    }
    out.clear();
    match read("/proc/self", &mut out, usize::MAX) {
        Ok(()) => log::warn!("unispace: read /proc/self unexpectedly succeeded"),
        Err(e) => log::info!(
            "unispace: read /proc/self -> {:?} (expected, no task yet)",
            e
        ),
    }

    // Method input schema on a folder: read(/A:mkdir).
    out.clear();
    match read("/A:mkdir", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_method_bytes(&out) {
            Ok((name, input, output)) => {
                SerialPort::puts("read(/A:mkdir) = method ");
                SerialPort::puts(&name);
                SerialPort::puts(" in ");
                SerialPort::puts(&schema::text_of_owned(&input));
                SerialPort::puts(" out ");
                SerialPort::puts(&schema::text_of_owned(&output));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: decode /A:mkdir failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /A:mkdir failed: {:?}", e),
    }

    // Object schema on a system root: read(/sys:desc).
    out.clear();
    match read("/sys:desc", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_object_bytes(&out) {
            Ok((kind, value, methods)) => {
                SerialPort::puts("read(/sys:desc) = kind ");
                SerialPort::puts(
                    ObjectKind::from_tag(kind)
                        .map(|k| k.as_str())
                        .unwrap_or("?"),
                );
                SerialPort::puts(" value ");
                SerialPort::puts(&schema::text_of_owned(&value));
                SerialPort::puts(" methods ");
                SerialPort::puts(&u64_short(methods.len() as u64));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: decode /sys:desc failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /sys:desc failed: {:?}", e),
    }

    // Build a tmpfs exercise tree via method dispatch.
    let mut payload = Vec::new();
    let name_val = |n: &str| Value::Struct(vec![Value::Str(String::from(n))]);
    let s_create: &'static Schema = &provider::vfs::CREATE_INPUT;

    match write_method(
        "/A:mkdir",
        &name_val("nos_test"),
        s_create,
        &mut payload,
        &mut out,
    ) {
        Ok(()) => SerialPort::puts("write(/A:mkdir {nos_test}) = ok\n"),
        Err(e) => log::warn!("unispace: mkdir nos_test failed: {:?}", e),
    }

    match write_method(
        "/A/nos_test:mkdir",
        &name_val("sub"),
        s_create,
        &mut payload,
        &mut out,
    ) {
        Ok(()) => SerialPort::puts("write(/A/nos_test:mkdir {sub}) = ok\n"),
        Err(e) => log::warn!("unispace: mkdir sub failed: {:?}", e),
    }

    match write_method(
        "/A/nos_test:create",
        &name_val("file"),
        s_create,
        &mut payload,
        &mut out,
    ) {
        Ok(()) => SerialPort::puts("write(/A/nos_test:create {file}) = ok\n"),
        Err(e) => log::warn!("unispace: create file failed: {:?}", e),
    }

    // File content: write replaces, read back.
    let content = b"hello from unispace!";
    match write("/A/nos_test/file", content, &mut out) {
        Ok(()) => SerialPort::puts("write(/A/nos_test/file) = ok\n"),
        Err(e) => log::warn!("unispace: file write failed: {:?}", e),
    }
    out.clear();
    match read("/A/nos_test/file", &mut out, usize::MAX) {
        Ok(()) => {
            SerialPort::puts("read(/A/nos_test/file) = ");
            SerialPort::puts(core::str::from_utf8(&out).unwrap_or("<non-utf8>"));
            SerialPort::puts("\n");
        }
        Err(e) => log::warn!("unispace: file read failed: {:?}", e),
    }

    // Folder-targeted method: stat on the file.
    let s_stat: &'static Schema = &provider::vfs::STAT_OUTPUT;
    out.clear();
    match write("/A/nos_test/file:stat", &[], &mut out) {
        Ok(()) => match schema::decode_value(&out, s_stat) {
            Ok(v) => {
                SerialPort::puts("write(/A/nos_test/file:stat) = ");
                SerialPort::puts(&schema::value_text(&v, s_stat));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: stat decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: stat failed: {:?}", e),
    }

    // sys provider: non-filesystem objects.
    out.clear();
    match read("/sys/version", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &schema::SCHEMA_STR) {
            Ok(v) => {
                SerialPort::puts("read(/sys/version) = ");
                SerialPort::puts(&schema::value_text(&v, &schema::SCHEMA_STR));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: version decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /sys/version failed: {:?}", e),
    }

    out.clear();
    match read("/sys/phys_mem", &mut out, usize::MAX) {
        Ok(()) => {
            let s = &provider::sys::PHYS_MEM;
            match schema::decode_value(&out, s) {
                Ok(v) => {
                    SerialPort::puts("read(/sys/phys_mem) = ");
                    SerialPort::puts(&schema::value_text(&v, s));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: phys_mem decode failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: read /sys/phys_mem failed: {:?}", e),
    }

    out.clear();
    match read("/sys/cpus", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &schema::SCHEMA_U32) {
            Ok(v) => {
                SerialPort::puts("read(/sys/cpus) = ");
                SerialPort::puts(&schema::value_text(&v, &schema::SCHEMA_U32));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: cpus decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /sys/cpus failed: {:?}", e),
    }

    // /kernel provider: monotonic timer object (uptime) and its :sleep method
    // input schema.  The blocking methods are not invoked from boot context.
    out.clear();
    match read("/kernel/timer", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &schema::SCHEMA_U64) {
            Ok(v) => {
                SerialPort::puts("read(/kernel/timer) = ");
                SerialPort::puts(&schema::value_text(&v, &schema::SCHEMA_U64));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: timer decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /kernel/timer failed: {:?}", e),
    }

    out.clear();
    match read("/kernel/timer:sleep", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_method_bytes(&out) {
            Ok((name, input, output)) => {
                SerialPort::puts("read(/kernel/timer:sleep) = method ");
                SerialPort::puts(&name);
                SerialPort::puts(" in ");
                SerialPort::puts(&schema::text_of_owned(&input));
                SerialPort::puts(" out ");
                SerialPort::puts(&schema::text_of_owned(&output));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: timer:sleep decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /kernel/timer:sleep failed: {:?}", e),
    }

    // /input provider: the UInputL event surface.  Runs before the scheduler
    // smoke test, so no task exists yet — the queue is drained empty and the
    // blocking `kbd:get` method is only schema-probed (never invoked).
    out.clear();
    match read("/input", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &DIR_SCHEMA) {
            Ok(v) => {
                SerialPort::puts("read(/input) = ");
                SerialPort::puts(&schema::value_text(&v, &DIR_SCHEMA));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: /input listing decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /input failed: {:?}", e),
    }

    out.clear();
    match read("/input/devices", &mut out, usize::MAX) {
        Ok(()) => {
            let s = &provider::input::DEVICE_LIST;
            match schema::decode_value(&out, s) {
                Ok(v) => {
                    SerialPort::puts("read(/input/devices) = ");
                    SerialPort::puts(&schema::value_text(&v, s));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: /input/devices decode failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: read /input/devices failed: {:?}", e),
    }

    out.clear();
    match read("/input/events", &mut out, usize::MAX) {
        Ok(()) => {
            let s = &provider::input::EVENT_LIST;
            match schema::decode_value(&out, s) {
                Ok(v) => {
                    SerialPort::puts("read(/input/events) = ");
                    SerialPort::puts(&schema::value_text(&v, s));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: /input/events decode failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: read /input/events failed: {:?}", e),
    }

    out.clear();
    match read("/input/kbd", &mut out, usize::MAX) {
        Ok(()) => {
            let s = &provider::input::KBD_STATE;
            match schema::decode_value(&out, s) {
                Ok(v) => {
                    SerialPort::puts("read(/input/kbd) = ");
                    SerialPort::puts(&schema::value_text(&v, s));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: /input/kbd decode failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: read /input/kbd failed: {:?}", e),
    }

    out.clear();
    match read("/input/kbd:get", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_method_bytes(&out) {
            Ok((name, input, output)) => {
                SerialPort::puts("read(/input/kbd:get) = method ");
                SerialPort::puts(&name);
                SerialPort::puts(" in ");
                SerialPort::puts(&schema::text_of_owned(&input));
                SerialPort::puts(" out ");
                SerialPort::puts(&schema::text_of_owned(&output));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: /input/kbd:get decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /input/kbd:get failed: {:?}", e),
    }

    // /driver provider: the audio device surface.  The value is a live
    // snapshot (ready state may be false on riscv64, where no controller
    // exists); the playback methods are only schema-probed here — invoking
    // them from boot context runs before the pump task exists and would fall
    // back to the blocking one-shot path for the tone's whole duration.
    #[cfg(target_arch = "x86_64")]
    {
        out.clear();
        match read("/driver/audio", &mut out, usize::MAX) {
            Ok(()) => {
                let s = &provider::driver::AUDIO_STATE;
                match schema::decode_value(&out, s) {
                    Ok(v) => {
                        SerialPort::puts("read(/driver/audio) = ");
                        SerialPort::puts(&schema::value_text(&v, s));
                        SerialPort::puts("\n");
                    }
                    Err(e) => log::warn!("unispace: /driver/audio decode failed: {:?}", e),
                }
            }
            Err(e) => log::warn!("unispace: read /driver/audio failed: {:?}", e),
        }

        out.clear();
        match read("/driver/audio:play_tone", &mut out, usize::MAX) {
            Ok(()) => match schema::decode_method_bytes(&out) {
                Ok((name, input, output)) => {
                    SerialPort::puts("read(/driver/audio:play_tone) = method ");
                    SerialPort::puts(&name);
                    SerialPort::puts(" in ");
                    SerialPort::puts(&schema::text_of_owned(&input));
                    SerialPort::puts(" out ");
                    SerialPort::puts(&schema::text_of_owned(&output));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: /driver/audio:play_tone decode failed: {:?}", e),
            },
            Err(e) => log::warn!("unispace: read /driver/audio:play_tone failed: {:?}", e),
        }

        out.clear();
        match read("/driver/audio:play_pcm", &mut out, usize::MAX) {
            Ok(()) => match schema::decode_method_bytes(&out) {
                Ok((name, input, output)) => {
                    SerialPort::puts("read(/driver/audio:play_pcm) = method ");
                    SerialPort::puts(&name);
                    SerialPort::puts(" in ");
                    SerialPort::puts(&schema::text_of_owned(&input));
                    SerialPort::puts(" out ");
                    SerialPort::puts(&schema::text_of_owned(&output));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: /driver/audio:play_pcm decode failed: {:?}", e),
            },
            Err(e) => log::warn!("unispace: read /driver/audio:play_pcm failed: {:?}", e),
        }
    }

    // /dev provider: the framebuffer device.  The fb is registered during
    // `Kernel::init()` — before `register_all()` — so `present` is true on
    // both arches at self-test time.
    out.clear();
    match read("/dev", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &DIR_SCHEMA) {
            Ok(v) => {
                SerialPort::puts("read(/dev) = ");
                SerialPort::puts(&schema::value_text(&v, &DIR_SCHEMA));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: /dev listing decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /dev failed: {:?}", e),
    }

    out.clear();
    match read("/dev/fb:mode", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_method_bytes(&out) {
            Ok((name, input, output)) => {
                SerialPort::puts("read(/dev/fb:mode) = method ");
                SerialPort::puts(&name);
                SerialPort::puts(" in ");
                SerialPort::puts(&schema::text_of_owned(&input));
                SerialPort::puts(" out ");
                SerialPort::puts(&schema::text_of_owned(&output));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: /dev/fb:mode schema decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /dev/fb:mode schema failed: {:?}", e),
    }

    out.clear();
    match write("/dev/fb:mode", &[], &mut out) {
        Ok(()) => {
            let s = &provider::dev::FB_MODE;
            match schema::decode_value(&out, s) {
                Ok(v) => {
                    SerialPort::puts("write(/dev/fb:mode) = ");
                    SerialPort::puts(&schema::value_text(&v, s));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: /dev/fb:mode decode failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: write /dev/fb:mode failed: {:?}", e),
    }

    // Framebuffer pixel round-trip: write 16 bytes at offset 0, read them
    // back, then `:clear` and confirm the window zeroed.
    let mut payload = vec![0u8; 16];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = i as u8;
    }
    out.clear();
    match write("/dev/fb", &payload, &mut out) {
        Ok(()) => SerialPort::puts("write(/dev/fb, 16B@0) = ok\n"),
        Err(e) => log::warn!("unispace: /dev/fb write failed: {:?}", e),
    }
    out.clear();
    match read("/dev/fb", &mut out, 16) {
        Ok(()) if out == payload => SerialPort::puts("read(/dev/fb, 16B@0) = ok (match)\n"),
        Ok(()) => log::warn!("unispace: /dev/fb readback mismatch ({} bytes)", out.len()),
        Err(e) => log::warn!("unispace: /dev/fb read failed: {:?}", e),
    }
    out.clear();
    match write("/dev/fb:clear", &[], &mut out) {
        Ok(()) => {
            out.clear();
            match read("/dev/fb", &mut out, 16) {
                Ok(()) if out.iter().all(|&b| b == 0) => {
                    SerialPort::puts("write(/dev/fb:clear) = ok (zeroed)\n")
                }
                Ok(()) => log::warn!("unispace: /dev/fb not zeroed after :clear"),
                Err(e) => log::warn!("unispace: /dev/fb read after :clear failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: /dev/fb:clear failed: {:?}", e),
    }

    // Cleanup the exercise tree.
    let _ = write_method(
        "/A/nos_test:unlink",
        &name_val("file"),
        s_create,
        &mut payload,
        &mut out,
    );
    let _ = write_method(
        "/A/nos_test:rmdir",
        &name_val("sub"),
        s_create,
        &mut payload,
        &mut out,
    );
    match write_method(
        "/A:rmdir",
        &name_val("nos_test"),
        s_create,
        &mut payload,
        &mut out,
    ) {
        Ok(()) => SerialPort::puts("write(/A:rmdir {nos_test}) = ok\n"),
        Err(e) => log::warn!("unispace: rmdir nos_test failed: {:?}", e),
    }

    // ── Dynamic subsystem connect / disconnect ─────────────────────
    //
    // A kernel subsystem declares an object at runtime (no static Schema or
    // hand-written struct), connects it at a deep path (auto-creating the
    // intermediate dir), exercises it, then disconnects it.
    let obj = decl::Declare::new(ObjectKind::Service)
        .value(schema::owned_struct(vec![(
            String::from("value"),
            schema::OwnedSchema::U64,
        )]))
        .read(|out, _max| {
            let v = Value::Struct(vec![Value::U64(42)]);
            schema::encode_value_owned(
                &v,
                &schema::owned_struct(vec![(String::from("value"), schema::OwnedSchema::U64)]),
                out,
            )
        })
        .method(
            "bump",
            schema::OwnedSchema::U64,
            schema::OwnedSchema::U64,
            |v, out| {
                let n = match v {
                    Value::U64(n) => *n,
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                schema::encode_value_owned(
                    &Value::U64(n.saturating_add(1)),
                    &schema::OwnedSchema::U64,
                    out,
                )
            },
        )
        .build();

    match connect("/test/sub/obj", obj) {
        Ok(()) => SerialPort::puts("connect(/test/sub/obj) = ok\n"),
        Err(e) => log::warn!("unispace: connect /test/sub/obj failed: {:?}", e),
    }

    out.clear();
    match read("/test/", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_value(&out, &DIR_SCHEMA) {
            Ok(v) => {
                SerialPort::puts("read(/test) = ");
                SerialPort::puts(&schema::value_text(&v, &DIR_SCHEMA));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: decode /test failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /test failed: {:?}", e),
    }

    out.clear();
    match read("/test/sub/obj", &mut out, usize::MAX) {
        Ok(()) => {
            let s = schema::owned_struct(vec![(String::from("value"), schema::OwnedSchema::U64)]);
            match schema::decode_value_owned(&out, &s) {
                Ok(v) => {
                    SerialPort::puts("read(/test/sub/obj) = ");
                    SerialPort::puts(&schema::value_text_owned(&v, &s));
                    SerialPort::puts("\n");
                }
                Err(e) => log::warn!("unispace: decode /test/sub/obj failed: {:?}", e),
            }
        }
        Err(e) => log::warn!("unispace: read /test/sub/obj failed: {:?}", e),
    }

    out.clear();
    match read("/test/sub/obj:desc", &mut out, usize::MAX) {
        Ok(()) => match schema::decode_object_bytes(&out) {
            Ok((kind, value, methods)) => {
                SerialPort::puts("read(/test/sub/obj:desc) = kind ");
                SerialPort::puts(
                    ObjectKind::from_tag(kind)
                        .map(|k| k.as_str())
                        .unwrap_or("?"),
                );
                SerialPort::puts(" value ");
                SerialPort::puts(&schema::text_of_owned(&value));
                SerialPort::puts(" methods=");
                SerialPort::puts(&u64_short(methods.len() as u64));
                SerialPort::puts("\n");
            }
            Err(e) => log::warn!("unispace: decode /test/sub/obj:desc failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: read /test/sub/obj:desc failed: {:?}", e),
    }

    out.clear();
    let mut payload = Vec::new();
    schema::encode_value_owned(&Value::U64(10), &schema::OwnedSchema::U64, &mut payload)
        .expect("bump payload encode");
    match write("/test/sub/obj:bump", &payload, &mut out) {
        Ok(()) => match schema::decode_value_owned(&out, &schema::OwnedSchema::U64) {
            Ok(Value::U64(11)) => SerialPort::puts("write(/test/sub/obj:bump {10}) = 11\n"),
            Ok(v) => log::warn!("unispace: bump returned {:?}", v),
            Err(e) => log::warn!("unispace: bump decode failed: {:?}", e),
        },
        Err(e) => log::warn!("unispace: bump failed: {:?}", e),
    }

    match write("/test/sub/obj", b"junk-not-a-u64", &mut out) {
        Ok(()) => log::warn!("unispace: write /test/sub/obj unexpectedly succeeded"),
        Err(e) => log::info!(
            "unispace: write /test/sub/obj -> {:?} (expected SchemaMismatch)",
            e
        ),
    }

    match disconnect("/test/sub/obj") {
        Ok(true) => SerialPort::puts("disconnect(/test/sub/obj) = ok (removed)\n"),
        Ok(false) => log::warn!("unispace: disconnect /test/sub/obj had nothing to remove"),
        Err(e) => log::warn!("unispace: disconnect /test/sub/obj failed: {:?}", e),
    }

    out.clear();
    match read("/test/sub/obj", &mut out, usize::MAX) {
        Ok(()) => {
            log::warn!("unispace: read /test/sub/obj after disconnect unexpectedly succeeded")
        }
        Err(e) => log::info!(
            "unispace: read /test/sub/obj after disconnect -> {:?} (expected)",
            e
        ),
    }

    // Refuse to connect inside an immutable provider tree (VFS /A).
    match connect("/A/forced", decl::Declare::new(ObjectKind::File).build()) {
        Ok(()) => log::warn!("unispace: connect /A/forced unexpectedly succeeded"),
        Err(e) => log::info!(
            "unispace: connect /A/forced -> {:?} (expected Unsupported)",
            e
        ),
    }

    SerialPort::puts("[unispace] self-test done\n");
}

/// Encode a `struct{name: str}` payload and invoke it as a method write.
#[cfg(feature = "selftest")]
fn write_method(
    path: &str,
    v: &Value,
    s: &Schema,
    payload: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(), UnispaceError> {
    payload.clear();
    schema::encode_value(v, s, payload)?;
    out.clear();
    write(path, payload, out)
}

#[cfg(feature = "selftest")]
fn u64_short(n: u64) -> String {
    let mut s = String::new();
    use alloc::string::ToString as _;
    s.push_str(&n.to_string());
    s
}
