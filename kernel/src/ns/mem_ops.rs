//! `/res` — the physical-resource branch of the namespace root.
//!
//! `/res/:map` is the map-on-open surface: a task writes a 16-byte payload
//! `[va u64 LE][phys u64 LE]` to map one physical frame into its own address
//! space (the successor of the old `mem:region` cap).
//!
//! v1 authority posture: the only physical frames a task may legitimately map
//! are ones it *holds* (RegionFile holdings, the successor of `mem:region`
//! caps). That bookkeeping is not yet wired, so v1 **denies every request** —
//! the plumbing (payload validation, user-half bound, borrowed-range
//! recording) exists so a future `mm/phys_region.rs` gates on
//! `task.regions` here. The borrowed-range teardown path (`Task::borrowed`,
//! detached before `teardown_low_half`) is implemented regardless, so the
//! ordering trap is closed even while no mapping can be created.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::file_ops::{FileOps, OpDesc};
use crate::filesystems::vfs::types::{DirEntry, FileKind, RightsMask, Stat};
use crate::ns::resolve::USER_LIMIT;

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 45;
const OP_RES_MAP: u64 = 450;

static RES_OPS: [OpDesc; 1] = [OpDesc {
    name: ":map",
    rights: RightsMask::W,
    doc: "Map a physical frame into the caller's address space (write [va u64][phys u64]).",
}];

/// One `:map` op file.
pub struct ResOp {
    ino: u64,
}

impl FileOps for ResOp {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn write(&self, _offset: u64, data: &[u8]) -> Result<usize, VfsError> {
        if data.len() < 16 {
            return Err(VfsError::InvalidInput);
        }
        let va = u64::from_le_bytes(data[0..8].try_into().map_err(|_| VfsError::InvalidInput)?);
        let phys =
            u64::from_le_bytes(data[8..16].try_into().map_err(|_| VfsError::InvalidInput)?);
        res_map(va, phys)
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

/// Map-on-open: validate the request and, if the caller holds authority,
/// map `[phys, phys+4096)` at `[va, va+4096)` in the caller's own tables and
/// record the borrowed range for teardown-time detach.
fn res_map(va: u64, phys: u64) -> Result<usize, VfsError> {
    // Structural validation (never trust the caller).
    if va & 0xFFF != 0 || phys & 0xFFF != 0 {
        return Err(VfsError::InvalidInput);
    }
    if va >= USER_LIMIT || va.checked_add(4096).map_or(true, |end| end > USER_LIMIT) {
        return Err(VfsError::InvalidInput);
    }
    let task = crate::proc::current_task().ok_or(VfsError::NotSupported)?;
    let root = task.domain.page_root().ok_or(VfsError::NotSupported)?;

    // Authority: a task may map only frames it holds (RegionFile holdings).
    // The v1 successor machinery is not wired, so every request is denied.
    // When `mm/phys_region.rs` lands, gate on `task.regions` here and:
    //   Vmm::from_root(root).map_4k(alloc, va, phys, READ|WRITE|USER);
    //   task.borrowed.lock().push(BorrowedRange { va, size: 4096 });
    let _ = (root, task);
    Err(VfsError::NotSupported)
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /res — physical-resource branch.
pub struct ResRoot;

impl FileOps for ResRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![DirEntry {
            ino: OP_RES_MAP,
            name: alloc::string::String::from(":map"),
            file_kind: FileKind::Op,
            rights: RightsMask::W,
        }])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            ":map" => Ok(Arc::new(ResOp { ino: OP_RES_MAP })),
            _ => Err(VfsError::NotFound),
        }
    }

    fn ops(&self) -> &'static [OpDesc] {
        &RES_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_ROOT, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the canonical `/res` root.
pub fn res_root() -> Arc<dyn FileOps> {
    Arc::new(ResRoot)
}
