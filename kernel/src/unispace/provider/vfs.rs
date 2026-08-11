use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::types::FileType;
use crate::filesystems::vfs::DRIVE_MAP;

use super::super::schema::{self, MethodDesc, Schema, Value};
use super::super::{
    ListingEntry, Object, ObjectKind, UnispaceError, DIR_SCHEMA, KIND_VARIANTS,
};

// ── Method schemas ─────────────────────────────────────────────────────

pub static CREATE_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "name",
    ty: &schema::SCHEMA_STR,
}]);

pub static RENAME_INPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "old",
        ty: &schema::SCHEMA_STR,
    },
    schema::Field {
        name: "new",
        ty: &schema::SCHEMA_STR,
    },
]);

pub static TRUNCATE_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "len",
    ty: &schema::SCHEMA_U64,
}]);

pub static STAT_OUTPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "ino",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "size",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "kind",
        ty: &Schema::Enum(&KIND_VARIANTS),
    },
    schema::Field {
        name: "mtime",
        ty: &schema::SCHEMA_U64,
    },
]);

static DIR_METHODS: [MethodDesc; 5] = [
    MethodDesc { name: "create", input: &CREATE_INPUT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "mkdir", input: &CREATE_INPUT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "rmdir", input: &CREATE_INPUT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "unlink", input: &CREATE_INPUT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "rename", input: &RENAME_INPUT, output: &schema::SCHEMA_UNIT },
];

static FILE_METHODS: [MethodDesc; 2] = [
    MethodDesc { name: "stat", input: &schema::SCHEMA_UNIT, output: &STAT_OUTPUT },
    MethodDesc { name: "truncate", input: &TRUNCATE_INPUT, output: &schema::SCHEMA_UNIT },
];

// ── Provider registration ──────────────────────────────────────────────

/// Attach the VFS mounts at `/A` (tmpfs) and, if mounted, `/B` (ESP).
pub fn register() -> Result<(), UnispaceError> {
    for letter in ['A', 'B'] {
        if let Ok(ops) = mount_root_ops(letter) {
            let obj: Arc<dyn Object> = Arc::new(VfsDir { ops });
            super::super::register(&String::from(letter), obj)?;
        }
    }
    Ok(())
}

fn mount_root_ops(letter: char) -> Result<Arc<dyn InodeOps>, UnispaceError> {
    let mount = DRIVE_MAP.lookup(letter)?;
    let lock = mount.root.inode.lock();
    let inode = lock.as_ref().ok_or(UnispaceError::NotFound)?;
    Ok(inode.ops.clone())
}

fn wrap(ops: Arc<dyn InodeOps>) -> Arc<dyn Object> {
    match ops.file_type() {
        FileType::Directory => Arc::new(VfsDir { ops }),
        FileType::Regular => Arc::new(VfsFile { ops }),
    }
}

fn arg_str(v: &Value, idx: usize) -> Result<&str, UnispaceError> {
    match v {
        Value::Struct(fields) => match fields.get(idx) {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(UnispaceError::SchemaMismatch),
        },
        _ => Err(UnispaceError::SchemaMismatch),
    }
}

// ── Directory object ───────────────────────────────────────────────────

pub struct VfsDir {
    ops: Arc<dyn InodeOps>,
}

impl Object for VfsDir {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Dir
    }

    fn value_schema(&self) -> &'static Schema {
        &DIR_SCHEMA
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &DIR_METHODS
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        let child = self.ops.lookup(name).ok()?;
        Some(wrap(child))
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        let entries = self.ops.readdir()?;
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let kind = match e.file_type {
                FileType::Regular => ObjectKind::File,
                FileType::Directory => ObjectKind::Dir,
            };
            out.push(ListingEntry { name: e.name, kind });
        }
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let mut entries = Vec::new();
        self.list(&mut entries)?;
        super::super::encode_listing(entries, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::IsADirectory)
    }

    fn invoke(&self, method: usize, v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                let name = arg_str(&v, 0)?;
                self.ops.create(name)?;
                Ok(())
            }
            1 => {
                let name = arg_str(&v, 0)?;
                self.ops.mkdir(name)?;
                Ok(())
            }
            2 => {
                let name = arg_str(&v, 0)?;
                self.ops.rmdir(name)?;
                Ok(())
            }
            3 => {
                let name = arg_str(&v, 0)?;
                self.ops.unlink(name)?;
                Ok(())
            }
            4 => {
                let old = arg_str(&v, 0)?;
                let new = arg_str(&v, 1)?;
                self.ops.rename(old, new)?;
                Ok(())
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}

// ── File object ────────────────────────────────────────────────────────

pub struct VfsFile {
    ops: Arc<dyn InodeOps>,
}

const READ_CHUNK: usize = 65536;

impl Object for VfsFile {
    fn kind(&self) -> ObjectKind {
        ObjectKind::File
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_BLOB
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &FILE_METHODS
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let size = self.ops.size();
        let mut pos = 0u64;
        while pos < size {
            let want = ((size - pos) as usize).min(READ_CHUNK);
            let mut chunk = vec![0u8; want];
            let n = self.ops.read_at(pos, &mut chunk)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            pos += n as u64;
        }
        Ok(())
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        self.ops.truncate(0)?;
        let mut pos = 0usize;
        while pos < bytes.len() {
            let n = self.ops.write_at(pos as u64, &bytes[pos..])?;
            if n == 0 {
                return Err(UnispaceError::Vfs(VfsError::IOError));
            }
            pos += n;
        }
        Ok(())
    }

    fn invoke(&self, method: usize, v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                let st = self.ops.getattr()?;
                let kind = match st.file_type {
                    FileType::Regular => 0u32,
                    FileType::Directory => 1u32,
                };
                let value = Value::Struct(vec![
                    Value::U64(st.ino),
                    Value::U64(st.size),
                    Value::Enum(kind),
                    Value::U64(st.mtime),
                ]);
                schema::encode_value(&value, &STAT_OUTPUT, out)
            }
            1 => {
                let len = match v {
                    Value::Struct(fields) => match fields.get(0) {
                        Some(Value::U64(l)) => *l,
                        _ => return Err(UnispaceError::SchemaMismatch),
                    },
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                self.ops.truncate(len)?;
                Ok(())
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}
