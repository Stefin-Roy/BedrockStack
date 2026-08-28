use core::sync::atomic::{AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;
use crate::filesystems::vfs::irq::IrqMutex;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::types::{DirEntry, FileType, Stat};

use super::mount::TMPFS_BUDGET;

static NEXT_INO: AtomicU64 = AtomicU64::new(2);
const ROOT_INO: u64 = 1;

pub(super) enum TmpfsEntry {
    File {
        data: IrqMutex<Vec<u8>>,
    },
    Dir {
        children: IrqMutex<HashMap<String, Arc<TmpfsInode>>>,
    },
    /// Raw bytes: symlink targets are arbitrary paths and need not be UTF-8.
    Symlink {
        target: IrqMutex<Vec<u8>>,
    },
    Fifo,
}

pub(super) struct TmpfsInode {
    pub ino: u64,
    pub file_type: FileType,
    pub entry: TmpfsEntry,
    pub mtime: IrqMutex<u64>,
    pub mode: IrqMutex<u32>,
    pub size: AtomicU64,
    /// Shared superblock usage counter (bytes), kept in sync on write/truncate.
    pub used: Arc<AtomicU64>,
}

impl TmpfsInode {
    pub fn new_root(used: Arc<AtomicU64>) -> Self {
        TmpfsInode {
            ino: ROOT_INO,
            file_type: FileType::Directory,
            entry: TmpfsEntry::Dir {
                children: IrqMutex::new(HashMap::new()),
            },
            mtime: IrqMutex::new(0),
            mode: IrqMutex::new(0o755),
            size: AtomicU64::new(0),
            used,
        }
    }

    /// Refuse growth that would exceed the filesystem budget.  Without this
    /// the 64 MiB budget is only a statfs fiction and tmpfs can OOM the
    /// kernel.
    #[allow(dead_code)]
    fn check_budget(&self, growth: u64) -> Result<(), VfsError> {
        if growth == 0 {
            return Ok(());
        }
        let cur = self.used.load(Ordering::Relaxed);
        if cur.saturating_add(growth) > TMPFS_BUDGET {
            return Err(VfsError::NoSpace);
        }
        Ok(())
    }

    /// Atomically reserve `growth` bytes. Fails with NoSpace if the budget
    /// would be exceeded. Uses a CAS loop so concurrent growth cannot overshoot.
    fn reserve_budget(&self, growth: u64) -> Result<(), VfsError> {
        if growth == 0 {
            return Ok(());
        }
        loop {
            let cur = self.used.load(Ordering::Relaxed);
            let new = cur.checked_add(growth).ok_or(VfsError::NoSpace)?;
            if new > TMPFS_BUDGET {
                return Err(VfsError::NoSpace);
            }
            match self.used.compare_exchange_weak(
                cur,
                new,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
    }
}

impl Drop for TmpfsInode {
    fn drop(&mut self) {
        // Refund the budget for the bytes this inode still holds.  This
        // runs when the last Arc (directory entry + any open FDs) is
        // dropped, so open-unlinked files keep their space charged until
        // the last close, matching POSIX tmpfs semantics.
        let sz = match &self.entry {
            TmpfsEntry::File { data } => data.lock().len() as u64,
            TmpfsEntry::Symlink { target } => target.lock().len() as u64,
            _ => 0,
        };
        if sz > 0 {
            self.used.fetch_sub(sz, Ordering::Relaxed);
        }
    }
}

impl InodeOps for TmpfsInode {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        match &self.entry {
            TmpfsEntry::File { data } => {
                let data = data.lock();
                if offset >= data.len() as u64 {
                    return Ok(0);
                }
                let start = offset as usize;
                let count = core::cmp::min(buf.len(), data.len() - start);
                buf[..count].copy_from_slice(&data[start..start + count]);
                Ok(count)
            }
            TmpfsEntry::Symlink { target } => {
                let t = target.lock();
                if offset >= t.len() as u64 {
                    return Ok(0);
                }
                let start = offset as usize;
                let count = core::cmp::min(buf.len(), t.len() - start);
                buf[..count].copy_from_slice(&t[start..start + count]);
                Ok(count)
            }
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        match &self.entry {
            TmpfsEntry::File { data } => {
                let mut data = data.lock();
                let old_len = data.len() as u64;
                let end = offset as usize + buf.len();
                if end as u64 > old_len {
                    self.reserve_budget(end as u64 - old_len)?;
                }
                if end > data.len() {
                    data.resize(end, 0);
                }
                data[offset as usize..end].copy_from_slice(buf);
                let new_len = data.len() as u64;
                self.size.store(new_len, Ordering::Relaxed);
                // Growth already accounted by reserve_budget; only shrink needs refund.
                if new_len < old_len {
                    self.used.fetch_sub(old_len - new_len, Ordering::Relaxed);
                }
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                Ok(buf.len())
            }
            TmpfsEntry::Symlink { target } => {
                let mut t = target.lock();
                let old_len = t.len() as u64;
                // Raw byte semantics: offset 0 replaces the whole target,
                // otherwise write at offset with zero-fill.  No UTF-8
                // coercion -- targets are arbitrary bytes.
                if offset == 0 {
                    let new_len = buf.len() as u64;
                    if new_len > old_len {
                        self.reserve_budget(new_len - old_len)?;
                    }
                    *t = buf.to_vec();
                    let new_len = t.len() as u64;
                    self.size.store(new_len, Ordering::Relaxed);
                    if new_len < old_len {
                        self.used.fetch_sub(old_len - new_len, Ordering::Relaxed);
                    }
                } else {
                    let end = offset as usize + buf.len();
                    let new_len_pre = if t.len() < end { end as u64 } else { t.len() as u64 };
                    if new_len_pre > old_len {
                        self.reserve_budget(new_len_pre - old_len)?;
                    }
                    if t.len() < end {
                        t.resize(end, 0);
                    }
                    t[offset as usize..end].copy_from_slice(buf);
                    let new_len = t.len() as u64;
                    self.size.store(new_len, Ordering::Relaxed);
                    if new_len < old_len {
                        self.used.fetch_sub(old_len - new_len, Ordering::Relaxed);
                    }
                }
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                Ok(buf.len())
            }
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let children = children.lock();
                children
                    .get(name)
                    .map(|c| c.clone() as Arc<dyn InodeOps>)
                    .ok_or(VfsError::NotFound)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn create(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: FileType::Regular,
                    entry: TmpfsEntry::File {
                        data: IrqMutex::new(Vec::new()),
                    },
                    mtime: IrqMutex::new(crate::services::wallclock::now_secs()),
                    mode: IrqMutex::new(0o644),
                    size: AtomicU64::new(0),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn InodeOps>)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                // Removal drops the Arc; the inode's Drop will refund the
                // budget when the last reference (e.g. open FD) is gone,
                // which correctly keeps space charged for open-unlinked files.
                children.remove(name).ok_or(VfsError::NotFound)?;
                Ok(())
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: FileType::Directory,
                    entry: TmpfsEntry::Dir {
                        children: IrqMutex::new(HashMap::new()),
                    },
                    mtime: IrqMutex::new(crate::services::wallclock::now_secs()),
                    mode: IrqMutex::new(0o755),
                    size: AtomicU64::new(0),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn InodeOps>)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                let child = children.get(name).ok_or(VfsError::NotFound)?;
                if let TmpfsEntry::Dir {
                    children: child_children,
                } = &child.entry
                {
                    if !child_children.lock().is_empty() {
                        return Err(VfsError::NotEmpty);
                    }
                }
                children.remove(name);
                Ok(())
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let children = children.lock();
                let mut entries = Vec::with_capacity(children.len());
                for (name, inode) in children.iter() {
                    entries.push(DirEntry {
                        ino: inode.ino,
                        name: name.clone(),
                        file_type: inode.file_type,
                    });
                }
                Ok(entries)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        let size = match &self.entry {
            TmpfsEntry::File { .. } => self.size.load(Ordering::Relaxed),
            TmpfsEntry::Symlink { target } => target.lock().len() as u64,
            _ => 0,
        };
        Ok(Stat {
            ino: self.ino,
            size,
            file_type: self.file_type,
            mtime: *self.mtime.lock(),
            mode: *self.mode.lock(),
        })
    }

    fn truncate(&self, len: u64) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::File { data } => {
                let mut data = data.lock();
                let old_len = data.len() as u64;
                if len > old_len {
                    self.reserve_budget(len - old_len)?;
                }
                data.resize(len as usize, 0);
                let new_len = data.len() as u64;
                self.size.store(len, Ordering::Relaxed);
                if new_len < old_len {
                    self.used.fetch_sub(old_len - new_len, Ordering::Relaxed);
                }
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                Ok(())
            }
            TmpfsEntry::Symlink { target } => {
                let mut t = target.lock();
                let old_len = t.len() as u64;
                if len > old_len {
                    self.reserve_budget(len - old_len)?;
                    t.resize(len as usize, 0);
                } else {
                    t.truncate(len as usize);
                }
                let new_len = t.len() as u64;
                self.size.store(new_len, Ordering::Relaxed);
                if new_len < old_len {
                    self.used.fetch_sub(old_len - new_len, Ordering::Relaxed);
                }
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                Ok(())
            }
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn chmod(&self, mode: u32) -> Result<(), VfsError> {
        *self.mode.lock() = mode & 0o7777;
        *self.mtime.lock() = crate::services::wallclock::now_secs();
        Ok(())
    }

    fn chown(&self, _uid: u32, _gid: u32) -> Result<(), VfsError> {
        // No ownership model; succeed and bump mtime.
        *self.mtime.lock() = crate::services::wallclock::now_secs();
        Ok(())
    }

    fn utimens(&self, mtime: u64) -> Result<(), VfsError> {
        *self.mtime.lock() = mtime;
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
                self.reserve_budget(target.len() as u64)?;
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: FileType::Symlink,
                    entry: TmpfsEntry::Symlink {
                        target: IrqMutex::new(target.as_bytes().to_vec()),
                    },
                    mtime: IrqMutex::new(crate::services::wallclock::now_secs()),
                    mode: IrqMutex::new(0o777),
                    size: AtomicU64::new(target.len() as u64),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn InodeOps>)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn readlink(&self) -> Result<String, VfsError> {
        match &self.entry {
            TmpfsEntry::Symlink { target } => {
                // The readlink API is String-typed; non-UTF-8 targets (which
                // raw storage now preserves) are rendered lossily here only.
                Ok(String::from_utf8_lossy(&target.lock()).into_owned())
            }
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn link(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if !children.contains_key(old_name) { return Err(VfsError::NotFound); }
                if children.contains_key(new_name) { return Err(VfsError::AlreadyExists); }
                let inode = children.get(old_name).ok_or(VfsError::NotFound)?.clone();
                // Hard link: share inode (increase nlink conceptually, but we just clone Arc).
                children.insert(String::from(new_name), inode);
                Ok(())
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn mknod(&self, name: &str, mode: u32, _dev: u64) -> Result<Arc<dyn InodeOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) { return Err(VfsError::AlreadyExists); }
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                // For now treat mknod as creating a FIFO/char device placeholder as regular file with mode bits.
                let ft = if mode & 0o60000 == 0o60000 { FileType::BlockDevice }
                    else if mode & 0o20000 == 0o20000 { FileType::CharDevice }
                    else if mode & 0o140000 == 0o140000 { FileType::Socket }
                    else { FileType::Regular };
                let entry = if ft == FileType::Fifo { TmpfsEntry::Fifo } else { TmpfsEntry::File { data: IrqMutex::new(Vec::new()) } };
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: ft,
                    entry,
                    mtime: IrqMutex::new(crate::services::wallclock::now_secs()),
                    mode: IrqMutex::new(mode & 0o7777),
                    size: AtomicU64::new(0),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn InodeOps>)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn mkfifo(&self, name: &str, mode: u32) -> Result<Arc<dyn InodeOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) { return Err(VfsError::AlreadyExists); }
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: FileType::Fifo,
                    entry: TmpfsEntry::Fifo,
                    mtime: IrqMutex::new(crate::services::wallclock::now_secs()),
                    mode: IrqMutex::new(mode & 0o777),
                    size: AtomicU64::new(0),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn InodeOps>)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                let child = children.remove(old_name).ok_or(VfsError::NotFound)?;
                children.insert(String::from(new_name), child);
                Ok(())
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn rename_across_dirs(
        &self,
        new_dir: &dyn InodeOps,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), VfsError> {
        let other = new_dir
            .as_any()
            .and_then(|a| a.downcast_ref::<TmpfsInode>())
            .ok_or(VfsError::CrossDeviceLink)?;
        if !Arc::ptr_eq(&self.used, &other.used) {
            return Err(VfsError::CrossDeviceLink);
        }
        if self.file_type != FileType::Directory || other.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return Err(VfsError::InvalidInput);
        }
        // Lock both directories in deterministic order by ino to avoid deadlock.
        if self.ino == other.ino {
            // Same directory; VFS would have taken same_parent path, but handle anyway.
            let children = match &self.entry {
                TmpfsEntry::Dir { children } => children,
                _ => return Err(VfsError::NotADirectory),
            };
            let mut g = children.lock();
            let child = g.remove(old_name).ok_or(VfsError::NotFound)?;
            g.insert(String::from(new_name), child);
            return Ok(());
        }
        let (first, second, self_is_first) = if self.ino < other.ino {
            (self, other, true)
        } else {
            (other, self, false)
        };
        let first_children = match &first.entry {
            TmpfsEntry::Dir { children } => children,
            _ => return Err(VfsError::NotADirectory),
        };
        let second_children = match &second.entry {
            TmpfsEntry::Dir { children } => children,
            _ => return Err(VfsError::NotADirectory),
        };
        let mut g1 = first_children.lock();
        let mut g2 = second_children.lock();
        let (src_map, dst_map) = if self_is_first {
            (&mut *g1, &mut *g2)
        } else {
            (&mut *g2, &mut *g1)
        };
        let child = src_map.remove(old_name).ok_or(VfsError::NotFound)?;
        // Overwrite destination if exists (same semantics as same-dir rename).
        dst_map.insert(String::from(new_name), child);
        Ok(())
    }

    fn file_type(&self) -> FileType {
        self.file_type
    }
    fn ino(&self) -> u64 {
        self.ino
    }
    fn size(&self) -> u64 {
        self.size.load(Ordering::Relaxed)
    }
}
