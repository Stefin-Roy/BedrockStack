use alloc::sync::Arc;
use alloc::vec::Vec;

use super::error::VfsError;
use super::types::{marshal_dir_entries, DirEntry, FileKind, RightsMask, Stat};

/// Static descriptor for an operation exposed by a `FileOps` implementation.
/// Used by the rights framework (future) and by the object layer to advertise
/// what hooks a file-like capability supports.
pub struct OpDesc {
    pub name: &'static str,
    pub rights: RightsMask,
    pub doc: &'static str,
}

/// Unified, rights-aware contract for every VFS node — on-disk inodes
/// (tmpfs, FAT32) and synthetic trees both implement this trait.
///
/// Method names `read`/`write` are the canonical I/O entry points; all
/// call sites use them (the historical `read_at`/`write_at` aliases are gone).
pub trait FileOps: Send + Sync {
    // ── Data I/O ──────────────────────────────────────────────────────
    /// Default `read` for a directory: the stateless full-listing readdir
    /// choke point.  Every entry is re-serialized into the binary wire format
    /// on each call and the result is sliced at `offset` (row boundaries are
    /// byte-aligned only when the caller passes a row-aligned offset).  This
    /// lets a reader stream a directory listing through the same `read` path
    /// as a regular file, with no server-side cursor state.  Non-directories
    /// fall through to `NotSupported` unless the implementor overrides.
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.file_kind() != FileKind::Directory {
            return Err(VfsError::NotSupported);
        }
        let entries = self.readdir()?;
        let data = marshal_dir_entries(&entries);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= data.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), data.len() - start);
        if n > 0 {
            buf[..n].copy_from_slice(&data[start..start + n]);
        }
        Ok(n)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::NotSupported)
    }

    // ── Size management ───────────────────────────────────────────────
    fn truncate(&self, len: u64) -> Result<(), VfsError> {
        Err(VfsError::NotSupported)
    }

    // ── Namespace operations ──────────────────────────────────────────
    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        Err(VfsError::NotADirectory)
    }
    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Err(VfsError::NotADirectory)
    }
    fn create(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        Err(VfsError::NotSupported)
    }
    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        Err(VfsError::NotSupported)
    }
    fn mkdir(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        Err(VfsError::NotSupported)
    }
    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        Err(VfsError::NotSupported)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        Err(VfsError::NotSupported)
    }

    // ── Metadata ──────────────────────────────────────────────────────
    fn getattr(&self) -> Result<Stat, VfsError> {
        Err(VfsError::NotSupported)
    }

    /// Static descriptor table for operations a caller may invoke with the
    /// appropriate rights.  Returns an empty slice by default.
    fn ops(&self) -> &'static [OpDesc] {
        &[]
    }

    // ── Identity ──────────────────────────────────────────────────────
    fn file_kind(&self) -> FileKind;
    fn ino(&self) -> u64;
    fn size(&self) -> u64 {
        0
    }

    /// Downcast hook so the VFS can recover the concrete implementor when
    /// needed (e.g. for synthetic trees that need private state access).
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// Called by VFS before the inode is removed from the namespace
    /// (unlink, rmdir).  The filesystem can mark the inode for deferred
    /// cleanup — the inode won't be dropped until the last open handle
    /// is closed.
    fn on_unlink(&self) {}
}
