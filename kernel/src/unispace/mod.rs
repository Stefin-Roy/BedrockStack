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
//! ## Discipline
//!
//! Every path, payload, and schema is validated with `Result` — malformed
//! input may only produce `Err`, never a panic (`panic = "abort"`).

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

use schema::{MethodDesc, Schema, Value};

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
    PermissionDenied,
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
pub trait Object: Send + Sync {
    fn kind(&self) -> ObjectKind;
    fn value_schema(&self) -> &'static Schema;
    fn methods(&self) -> &'static [MethodDesc];

    /// Directory capability (leaf default: no children).
    fn resolve(&self, _name: &str) -> Option<Arc<dyn Object>> {
        None
    }

    fn list(&self, _out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError>;

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::PermissionDenied)
    }

    fn invoke(&self, _method: usize, _v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        Err(UnispaceError::MethodNotFound)
    }
}

// ── Shared schema for directory listings ───────────────────────────────

pub(crate) static KIND_VARIANTS: [schema::EnumVariant; 4] = [
    schema::EnumVariant { name: "file", value: 0 },
    schema::EnumVariant { name: "dir", value: 1 },
    schema::EnumVariant { name: "device", value: 2 },
    schema::EnumVariant { name: "service", value: 3 },
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
    let mut items = Vec::with_capacity(entries.len());
    for e in entries {
        items.push(Value::Struct(vec![
            Value::Str(e.name),
            Value::Enum(e.kind.tag() as u32),
        ]));
    }
    let root = Value::Struct(vec![Value::List(items)]);
    schema::encode_value(&root, &DIR_SCHEMA, out)
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
    if name.is_empty()
        || name.contains('/')
        || name.contains(':')
        || name == "."
        || name == ".."
    {
        return Err(UnispaceError::InvalidPath);
    }
    root().insert(name, obj);
    Ok(())
}

/// Detach a system (e.g. a hot-plugged drive).
pub fn unregister(name: &str) -> bool {
    root().remove(name)
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
    obj.methods().iter().position(|m| m.name == name)
}

/// Read an object's value, or (with `:method`/`:desc`) a schema.
pub fn read(path: &str, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    let (obj, method) = resolve(path)?;
    match method {
        Some("desc") => encode_object_desc(&*obj, out),
        Some(m) => {
            let idx = find_method(&*obj, m).ok_or(UnispaceError::MethodNotFound)?;
            let md = &obj.methods()[idx];
            encode_method_desc(md, out)
        }
        None => {
            if obj.kind() == ObjectKind::Dir {
                let mut entries = Vec::new();
                obj.list(&mut entries)?;
                encode_listing(entries, out)
            } else {
                obj.read_value(out)
            }
        }
    }
}

/// Write an object's value, or invoke a method.  Method output (if any) is
/// appended to `out`; value writes leave it untouched.
pub fn write(path: &str, data: &[u8], out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    let (obj, method) = resolve(path)?;
    match method {
        Some("desc") => Err(UnispaceError::PermissionDenied),
        Some(m) => {
            let idx = find_method(&*obj, m).ok_or(UnispaceError::MethodNotFound)?;
            let md = &obj.methods()[idx];
            let value = schema::decode_value(data, md.input)?;
            obj.invoke(idx, value, out)
        }
        None => {
            if obj.kind() == ObjectKind::Dir {
                return Err(UnispaceError::IsADirectory);
            }
            let value = schema::decode_value(data, obj.value_schema())?;
            obj.write_value(value)
        }
    }
}

fn encode_object_desc(obj: &dyn Object, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    out.push(obj.kind().tag());
    schema::encode_schema(obj.value_schema(), out);
    let methods = obj.methods();
    out.extend_from_slice(&(methods.len() as u32).to_le_bytes());
    for md in methods {
        encode_method_desc(md, out)?;
    }
    Ok(())
}

fn encode_method_desc(md: &MethodDesc, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    schema::write_len_string(out, md.name);
    schema::encode_schema(md.input, out);
    schema::encode_schema(md.output, out);
    Ok(())
}

// ── Boot self-test ─────────────────────────────────────────────────────

/// Exercise read/write/schema dispatch over the registered providers and print
/// results to serial.  Non-fatal: every step logs its own outcome.
pub fn self_test() {
    use crate::drivers::serial::SerialPort;
    SerialPort::puts("[unispace] self-test start\n");
    let mut out = Vec::new();

    // Listing of the root (the system registry).
    match read("/", &mut out) {
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

    // Method input schema on a folder: read(/A:mkdir).
    out.clear();
    match read("/A:mkdir", &mut out) {
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
    match read("/sys:desc", &mut out) {
        Ok(()) => match schema::decode_object_bytes(&out) {
            Ok((kind, value, methods)) => {
                SerialPort::puts("read(/sys:desc) = kind ");
                SerialPort::puts(ObjectKind::from_tag(kind).map(|k| k.as_str()).unwrap_or("?"));
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

    match write_method("/A:mkdir", &name_val("nos_test"), s_create, &mut payload, &mut out) {
        Ok(()) => SerialPort::puts("write(/A:mkdir {nos_test}) = ok\n"),
        Err(e) => log::warn!("unispace: mkdir nos_test failed: {:?}", e),
    }

    match write_method("/A/nos_test:mkdir", &name_val("sub"), s_create, &mut payload, &mut out) {
        Ok(()) => SerialPort::puts("write(/A/nos_test:mkdir {sub}) = ok\n"),
        Err(e) => log::warn!("unispace: mkdir sub failed: {:?}", e),
    }

    match write_method("/A/nos_test:create", &name_val("file"), s_create, &mut payload, &mut out) {
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
    match read("/A/nos_test/file", &mut out) {
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
    match read("/sys/version", &mut out) {
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
    match read("/sys/phys_mem", &mut out) {
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
    match read("/sys/cpus", &mut out) {
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

    // Cleanup the exercise tree.
    let _ = write_method("/A/nos_test:unlink", &name_val("file"), s_create, &mut payload, &mut out);
    let _ = write_method("/A/nos_test:rmdir", &name_val("sub"), s_create, &mut payload, &mut out);
    match write_method("/A:rmdir", &name_val("nos_test"), s_create, &mut payload, &mut out) {
        Ok(()) => SerialPort::puts("write(/A:rmdir {nos_test}) = ok\n"),
        Err(e) => log::warn!("unispace: rmdir nos_test failed: {:?}", e),
    }

    SerialPort::puts("[unispace] self-test done\n");
}

/// Encode a `struct{name: str}` payload and invoke it as a method write.
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

fn u64_short(n: u64) -> String {
    let mut s = String::new();
    use alloc::string::ToString as _;
    s.push_str(&n.to_string());
    s
}
