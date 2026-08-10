//! /mnt — mount root tree.
//!
//! Synthetic VFS tree over the name-keyed mount registry (`vfs::list_mounts`).
//! Each mounted filesystem appears as a subdirectory named after its mount
//! point (e.g. `A`, `esp`); descending into one delegates to the mounted
//! filesystem's root, so `/mnt/esp/EFI/BEDROCK/INIT` walks the ESP.  The
//! `/mnt` root also advertises `:mount` / `:unmount` op files (write-only,
//! structure-only this phase; real mount/unmount dispatch is wired in
//! Phase 5 alongside the namespace).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::get_mount;
use super::super::list_mounts;
use super::super::mount::DriveMount;
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 50;
const INO_MOUNT_OP: u64 = 500;
const INO_UNMOUNT_OP: u64 = 501;

static MNT_OPS: [OpDesc; 2] = [
    OpDesc {
        name: ":mount",
        rights: RightsMask::W,
        doc: "Mount a filesystem at a named mount point (write request body).",
    },
    OpDesc {
        name: ":unmount",
        rights: RightsMask::W,
        doc: "Unmount a named mount point (write request body).",
    },
];

// ── Mount/op node ────────────────────────────────────────────────────────

/// A single mount-control op file (`:mount` / `:unmount`).  Structure-only
/// this phase; dispatch wired in Phase 5.
pub struct MntOp {
    ino: u64,
}

impl FileOps for MntOp {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

// ── Mounted filesystem entry ─────────────────────────────────────────────

/// `/mnt/<name>` — a mounted filesystem.  Namespace operations delegate to
/// the mount root's `FileOps`, so children are the actual on-disk nodes.
pub struct MountEntry {
    mount: Arc<DriveMount>,
}

impl MountEntry {
    fn root_ops(&self) -> Option<Arc<dyn FileOps>> {
        self.mount.root.inode.lock().as_ref().map(|inode| inode.ops.clone())
    }
}

impl FileOps for MountEntry {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        self.mount.id
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        self.root_ops().ok_or(VfsError::IOError)?.readdir()
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        self.root_ops().ok_or(VfsError::IOError)?.lookup(name)
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        let kind = self.root_ops().map(|ops| ops.file_kind()).unwrap_or(FileKind::Directory);
        Ok(Stat { ino: self.ino(), size: 0, file_kind: kind, mtime: 0 })
    }
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /mnt — mount root tree.
pub struct MntRoot;

impl FileOps for MntRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let mut entries = Vec::new();
        for name in list_mounts() {
            let ino = get_mount(&name).map(|m| m.id).unwrap_or(0);
            entries.push(DirEntry {
                ino,
                name,
                file_kind: FileKind::Directory,
                rights: RightsMask::RW,
            });
        }
        entries.push(DirEntry {
            ino: INO_MOUNT_OP,
            name: String::from(":mount"),
            file_kind: FileKind::Op,
            rights: RightsMask::W,
        });
        entries.push(DirEntry {
            ino: INO_UNMOUNT_OP,
            name: String::from(":unmount"),
            file_kind: FileKind::Op,
            rights: RightsMask::W,
        });
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            ":mount" => Ok(Arc::new(MntOp { ino: INO_MOUNT_OP })),
            ":unmount" => Ok(Arc::new(MntOp { ino: INO_UNMOUNT_OP })),
            _ => {
                let mount = get_mount(name).ok_or(VfsError::NotFound)?;
                Ok(Arc::new(MountEntry { mount }))
            }
        }
    }

    fn ops(&self) -> &'static [OpDesc] {
        &MNT_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_ROOT, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the canonical `/mnt` root.
pub fn mnt_root() -> Arc<dyn FileOps> {
    Arc::new(MntRoot)
}
