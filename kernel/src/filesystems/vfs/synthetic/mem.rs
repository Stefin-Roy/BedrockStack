//! /mem — physical / heap / addrspace memory control tree.
//!
//! Synthetic VFS tree exposing the memory family as three sub-directories
//! (`phys`, `heap`, `addrspace`), each of which advertises a table of
//! write-only *op* files (`:alloc`, `:free`, `:map`, ...).
//!
//! This phase builds the directory/op **structure only**: every op node
//! inherits the default `FileOps::read`/`write` which return
//! `VfsError::NotSupported`. The confinement policy that gates real
//! allocation/free/mapping is decided in a later phase — this module never
//! calls into the real memory manager.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 30;
const INO_PHYS: u64 = 31;
const INO_HEAP: u64 = 32;
const INO_ADDRSPACE: u64 = 33;

// Op inodes are grouped by family; phys 200.., heap 210.., addrspace 220..
const OP_PHYS_ALLOC: u64 = 200;
const OP_PHYS_FREE: u64 = 201;
const OP_PHYS_MAP: u64 = 202;

const OP_HEAP_ALLOC: u64 = 210;
const OP_HEAP_FREE: u64 = 211;

const OP_ADDR_MAP: u64 = 220;
const OP_ADDR_UNMAP: u64 = 221;
const OP_ADDR_PROTECT: u64 = 222;

// ── Op descriptor tables ─────────────────────────────────────────────────
// Returned by each family directory's `ops()` so the rights/object layer can
// advertise what control hooks exist without materialising op nodes eagerly.

static PHYS_OPS: [OpDesc; 3] = [
    OpDesc {
        name: ":alloc",
        rights: RightsMask::W,
        doc: "Request allocation of physical frame range (write request body).",
    },
    OpDesc {
        name: ":free",
        rights: RightsMask::W,
        doc: "Release a physical frame range (write request body).",
    },
    OpDesc {
        name: ":map",
        rights: RightsMask::W,
        doc: "Establish a phys->io mapping (write request body).",
    },
];

static HEAP_OPS: [OpDesc; 2] = [
    OpDesc {
        name: ":alloc",
        rights: RightsMask::W,
        doc: "Request heap allocation (write request body).",
    },
    OpDesc {
        name: ":free",
        rights: RightsMask::W,
        doc: "Release a heap allocation (write request body).",
    },
];

static ADDRSPACE_OPS: [OpDesc; 3] = [
    OpDesc {
        name: ":map",
        rights: RightsMask::W,
        doc: "Map a virtual range (write request body).",
    },
    OpDesc {
        name: ":unmap",
        rights: RightsMask::W,
        doc: "Unmap a virtual range (write request body).",
    },
    OpDesc {
        name: ":protect",
        rights: RightsMask::W,
        doc: "Change mapping protection (write request body).",
    },
];

// ── Op node ──────────────────────────────────────────────────────────────

/// A single memory-family op file. All ops share the same stub behaviour this
/// phase: `read`/`write` are `NotSupported` via the `FileOps` defaults and the
/// real dispatch is wired in Phase 5.
pub struct MemOp {
    ino: u64,
}

impl MemOp {
    const fn new(ino: u64) -> Self {
        MemOp { ino }
    }
}

impl FileOps for MemOp {
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

// ── Family directories ──────────────────────────────────────────────────

/// /mem/phys — physical memory control.
pub struct PhysDir;

impl FileOps for PhysDir {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_PHYS
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry {
                ino: OP_PHYS_ALLOC,
                name: String::from(":alloc"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_PHYS_FREE,
                name: String::from(":free"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_PHYS_MAP,
                name: String::from(":map"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let ino = match name {
            ":alloc" => OP_PHYS_ALLOC,
            ":free" => OP_PHYS_FREE,
            ":map" => OP_PHYS_MAP,
            _ => return Err(VfsError::NotFound),
        };
        Ok(Arc::new(MemOp::new(ino)))
    }

    fn ops(&self) -> &'static [OpDesc] {
        &PHYS_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: INO_PHYS,
            size: 0,
            file_kind: FileKind::Directory,
            mtime: 0,
        })
    }
}

/// /mem/heap — heap memory control.
pub struct HeapDir;

impl FileOps for HeapDir {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_HEAP
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry {
                ino: OP_HEAP_ALLOC,
                name: String::from(":alloc"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_HEAP_FREE,
                name: String::from(":free"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let ino = match name {
            ":alloc" => OP_HEAP_ALLOC,
            ":free" => OP_HEAP_FREE,
            _ => return Err(VfsError::NotFound),
        };
        Ok(Arc::new(MemOp::new(ino)))
    }

    fn ops(&self) -> &'static [OpDesc] {
        &HEAP_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: INO_HEAP,
            size: 0,
            file_kind: FileKind::Directory,
            mtime: 0,
        })
    }
}

/// /mem/addrspace — virtual address-space control.
pub struct AddrspaceDir;

impl FileOps for AddrspaceDir {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ADDRSPACE
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry {
                ino: OP_ADDR_MAP,
                name: String::from(":map"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_ADDR_UNMAP,
                name: String::from(":unmap"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
            DirEntry {
                ino: OP_ADDR_PROTECT,
                name: String::from(":protect"),
                file_kind: FileKind::Op,
                rights: RightsMask::W,
            },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let ino = match name {
            ":map" => OP_ADDR_MAP,
            ":unmap" => OP_ADDR_UNMAP,
            ":protect" => OP_ADDR_PROTECT,
            _ => return Err(VfsError::NotFound),
        };
        Ok(Arc::new(MemOp::new(ino)))
    }

    fn ops(&self) -> &'static [OpDesc] {
        &ADDRSPACE_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: INO_ADDRSPACE,
            size: 0,
            file_kind: FileKind::Directory,
            mtime: 0,
        })
    }
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /mem — top of the memory control tree. Exposes only the family
/// sub-directories; `ops()` is intentionally left to the default (empty slice)
/// since concrete control hooks live on the family directories.
pub struct MemRoot;

impl FileOps for MemRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry {
                ino: INO_PHYS,
                name: String::from("phys"),
                file_kind: FileKind::Directory,
                rights: RightsMask::R,
            },
            DirEntry {
                ino: INO_HEAP,
                name: String::from("heap"),
                file_kind: FileKind::Directory,
                rights: RightsMask::R,
            },
            DirEntry {
                ino: INO_ADDRSPACE,
                name: String::from("addrspace"),
                file_kind: FileKind::Directory,
                rights: RightsMask::R,
            },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            "phys" => Ok(Arc::new(PhysDir)),
            "heap" => Ok(Arc::new(HeapDir)),
            "addrspace" => Ok(Arc::new(AddrspaceDir)),
            _ => Err(VfsError::NotFound),
        }
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

pub fn mem_root() -> Arc<dyn FileOps> {
    Arc::new(MemRoot)
}
