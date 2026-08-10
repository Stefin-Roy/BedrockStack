use core::sync::atomic::{AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;
use spin::Mutex;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::file_ops::FileOps;
use crate::filesystems::vfs::types::{DirEntry, FileKind, RightsMask, Stat};

static NEXT_INO: AtomicU64 = AtomicU64::new(2);
const ROOT_INO: u64 = 1;

/// Reject names containing `:` — the colon is reserved by the kernel to flag
/// synthetic/synthetic-mapped files.  A real filesystem under tmpfs may never
/// create such an entry.
fn contains_colon(name: &str) -> bool {
    name.as_bytes().contains(&b':')
}

pub(super) enum TmpfsEntry {
    File { data: Mutex<Vec<u8>> },
    Dir { children: Mutex<HashMap<String, Arc<TmpfsInode>>> },
}

pub(super) struct TmpfsInode {
    pub ino: u64,
    pub file_kind: FileKind,
    pub entry: TmpfsEntry,
    pub mtime: Mutex<u64>,
    pub size: AtomicU64,
    /// Shared superblock usage counter (bytes), kept in sync on write/truncate.
    pub used: Arc<AtomicU64>,
}

impl TmpfsInode {
    pub fn new_root(used: Arc<AtomicU64>) -> Self {
        TmpfsInode {
            ino: ROOT_INO,
            file_kind: FileKind::Directory,
            entry: TmpfsEntry::Dir {
                children: Mutex::new(HashMap::new()),
            },
            mtime: Mutex::new(0),
            size: AtomicU64::new(0),
            used,
        }
    }
}

impl FileOps for TmpfsInode {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
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
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn write(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        match &self.entry {
            TmpfsEntry::File { data } => {
                let mut data = data.lock();
                let old_len = data.len() as u64;
                let end = (offset as usize).checked_add(buf.len()).ok_or(VfsError::FileTooLarge)?;
                if end > data.len() {
                    data.resize(end, 0);
                }
                data[offset as usize..end].copy_from_slice(buf);
                let new_len = data.len() as u64;
                self.size.store(new_len, Ordering::Relaxed);
                if new_len > old_len {
                    self.used.fetch_add(new_len - old_len, Ordering::Relaxed);
                }
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                Ok(buf.len())
            }
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let children = children.lock();
                children
                    .get(name)
                    .map(|c| c.clone() as Arc<dyn FileOps>)
                    .ok_or(VfsError::NotFound)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn create(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        if contains_colon(name) {
            return Err(VfsError::InvalidInput);
        }
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_kind: FileKind::Regular,
                    entry: TmpfsEntry::File {
                        data: Mutex::new(Vec::new()),
                    },
                    mtime: Mutex::new(0),
                    size: AtomicU64::new(0),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn FileOps>)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                let removed = children.remove(name).ok_or(VfsError::NotFound)?;
                if let TmpfsEntry::File { data } = &removed.entry {
                    self.used.fetch_sub(data.lock().len() as u64, Ordering::Relaxed);
                }
                Ok(())
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        if contains_colon(name) {
            return Err(VfsError::InvalidInput);
        }
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_kind: FileKind::Directory,
                    entry: TmpfsEntry::Dir {
                        children: Mutex::new(HashMap::new()),
                    },
                    mtime: Mutex::new(0),
                    size: AtomicU64::new(0),
                    used: self.used.clone(),
                });
                children.insert(String::from(name), child.clone());
                Ok(child as Arc<dyn FileOps>)
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
                        file_kind: inode.file_kind,
                        rights: RightsMask::R,
                    });
                }
                Ok(entries)
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: self.ino,
            size: self.size.load(Ordering::Relaxed),
            file_kind: self.file_kind,
            mtime: *self.mtime.lock(),
        })
    }

    fn truncate(&self, len: u64) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::File { data } => {
                let mut data = data.lock();
                let old_len = data.len() as u64;
                data.resize(len as usize, 0);
                let new_len = data.len() as u64;
                self.size.store(len, Ordering::Relaxed);
                if new_len > old_len {
                    self.used.fetch_add(new_len - old_len, Ordering::Relaxed);
                } else if new_len < old_len {
                    self.used.fetch_sub(old_len - new_len, Ordering::Relaxed);
                }
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                Ok(())
            }
            _ => Err(VfsError::IsADirectory),
        }
    }

    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        if contains_colon(new_name) {
            return Err(VfsError::InvalidInput);
        }
        if contains_colon(old_name) {
            return Err(VfsError::InvalidInput);
        }
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                let child = children.remove(old_name).ok_or(VfsError::NotFound)?;
                if let Some(replaced) = children.insert(String::from(new_name), child) {
                    if let TmpfsEntry::File { data } = &replaced.entry {
                        self.used.fetch_sub(data.lock().len() as u64, Ordering::Relaxed);
                    }
                }
                Ok(())
            }
            _ => Err(VfsError::NotADirectory),
        }
    }

    fn file_kind(&self) -> FileKind { self.file_kind }
    fn ino(&self) -> u64 { self.ino }
    fn size(&self) -> u64 { self.size.load(Ordering::Relaxed) }
}
