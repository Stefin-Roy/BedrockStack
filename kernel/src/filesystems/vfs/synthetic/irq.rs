//! /irq — interrupt-vector control tree.
//!
//! Synthetic VFS tree exposing the interrupt family as a single directory of
//! write-only (and one read-only) *op* files: `:register`, `:unregister`,
//! `:ack`, `:enable`, `:disable`, `:enum`.
//!
//! This phase builds the directory/op **structure only**: every op node
//! inherits the default `FileOps::read`/`write` which return
//! `VfsError::NotSupported`. The real interrupt services are NOT called here;
//! dispatch is wired in Phase 5.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 40;

// Op inodes start at 400 and follow the readdir order (400 + index).
const OP_REGISTER: u64 = 400;
const OP_UNREGISTER: u64 = 401;
const OP_ACK: u64 = 402;
const OP_ENABLE: u64 = 403;
const OP_DISABLE: u64 = 404;
const OP_ENUM: u64 = 405;

// ── Op descriptor table ──────────────────────────────────────────────────

static IRQ_OPS: [OpDesc; 6] = [
    OpDesc {
        name: ":register",
        rights: RightsMask::W,
        doc: "Bind a handler to an interrupt vector (write request body).",
    },
    OpDesc {
        name: ":unregister",
        rights: RightsMask::W,
        doc: "Release a previously registered handler (write request body).",
    },
    OpDesc {
        name: ":ack",
        rights: RightsMask::W,
        doc: "Acknowledge a pending interrupt (write request body).",
    },
    OpDesc {
        name: ":enable",
        rights: RightsMask::W,
        doc: "Enable delivery of an interrupt vector (write request body).",
    },
    OpDesc {
        name: ":disable",
        rights: RightsMask::W,
        doc: "Mask/disable an interrupt vector (write request body).",
    },
    OpDesc {
        name: ":enum",
        rights: RightsMask::R,
        doc: "Enumerate available interrupt vectors (read).",
    },
];

// ── Op node ──────────────────────────────────────────────────────────────

/// A single interrupt-family op file. All ops share the same stub behaviour
/// this phase: `read`/`write` are `NotSupported` via the `FileOps` defaults;
/// real interrupt services are wired in Phase 5.
pub struct IrqOp {
    ino: u64,
}

impl IrqOp {
    const fn new(ino: u64) -> Self {
        IrqOp { ino }
    }
}

impl FileOps for IrqOp {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: self.ino,
            size: 0,
            file_kind: FileKind::Op,
            mtime: 0,
        })
    }

    // read / write fall through to the trait defaults -> Err(NotSupported).
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /irq — interrupt control tree. Direct children are op files (no family
/// sub-directories); `ops()` advertises the full control surface.
pub struct IrqRoot;

impl FileOps for IrqRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry {
                ino: OP_REGISTER,
                name: String::from(":register"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_UNREGISTER,
                name: String::from(":unregister"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_ACK,
                name: String::from(":ack"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_ENABLE,
                name: String::from(":enable"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_DISABLE,
                name: String::from(":disable"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_ENUM,
                name: String::from(":enum"),
                file_kind: FileKind::Op,
                rights: RightsMask::R,
            },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let ino = match name {
            ":register" => OP_REGISTER,
            ":unregister" => OP_UNREGISTER,
            ":ack" => OP_ACK,
            ":enable" => OP_ENABLE,
            ":disable" => OP_DISABLE,
            ":enum" => OP_ENUM,
            _ => return Err(VfsError::NotFound),
        };
        Ok(Arc::new(IrqOp::new(ino)))
    }

    fn ops(&self) -> &'static [OpDesc] {
        &IRQ_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: INO_ROOT,
            size: 0,
            file_kind: FileKind::Directory,
            mtime: 0,
        })
    }
}

/// `/irq` does not carry real interrupt services in this phase; the handle is
/// retained so callers can construct the canonical `Arc<dyn FileOps>`.
pub fn irq_root() -> Arc<dyn FileOps> {
    Arc::new(IrqRoot)
}
