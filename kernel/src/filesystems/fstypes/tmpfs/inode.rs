use core::sync::atomic::{AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;
use spin::Mutex;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::types::{DirEntry, FileType, Stat};

static NEXT_INO: AtomicU64 = AtomicU64::new(2);
const ROOT_INO: u64 = 1;

pub(super) enum TmpfsEntry {
    File {
        data: Mutex<Vec<u8>>,
    },
    Dir {
        children: Mutex<HashMap<String, Arc<TmpfsInode>>>,
    },
    Symlink {
        target: Mutex<String>,
    },
    Fifo,
}

pub(super) struct TmpfsInode {
    pub ino: u64,
    pub file_type: FileType,
    pub entry: TmpfsEntry,
    pub mtime: Mutex<u64>,
    pub mode: Mutex<u32>,
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
                children: Mutex::new(HashMap::new()),
            },
            mtime: Mutex::new(0),
            mode: Mutex::new(0o755),
            size: AtomicU64::new(0),
            used,
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
                let bytes = t.as_bytes();
                if offset >= bytes.len() as u64 {
                    return Ok(0);
                }
                let start = offset as usize;
                let count = core::cmp::min(buf.len(), bytes.len() - start);
                buf[..count].copy_from_slice(&bytes[start..start + count]);
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
            TmpfsEntry::Symlink { target } => {
                let mut t = target.lock();
                let old_len = t.len() as u64;
                // For symlink, write replaces target string if offset==0, else append.
                if offset == 0 {
                    *t = String::from_utf8_lossy(buf).into_owned();
                } else {
                    // Simple: extend if needed.
                    let cur = t.clone();
                    let mut new = cur.into_bytes();
                    let end = offset as usize + buf.len();
                    if end > new.len() { new.resize(end, 0); }
                    new[offset as usize..end].copy_from_slice(buf);
                    *t = String::from_utf8_lossy(&new).into_owned();
                }
                let new_len = t.len() as u64;
                self.size.store(new_len, Ordering::Relaxed);
                *self.mtime.lock() = crate::services::wallclock::now_secs();
                let _ = old_len;
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
                        data: Mutex::new(Vec::new()),
                    },
                    mtime: Mutex::new(crate::services::wallclock::now_secs()),
                    mode: Mutex::new(0o644),
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
                        children: Mutex::new(HashMap::new()),
                    },
                    mtime: Mutex::new(crate::services::wallclock::now_secs()),
                    mode: Mutex::new(0o755),
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
            TmpfsEntry::Symlink { target } => {
                let mut t = target.lock();
                t.truncate(len as usize);
                self.size.store(len, Ordering::Relaxed);
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
                let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: FileType::Symlink,
                    entry: TmpfsEntry::Symlink { target: Mutex::new(String::from(target)) },
                    mtime: Mutex::new(crate::services::wallclock::now_secs()),
                    mode: Mutex::new(0o777),
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
            TmpfsEntry::Symlink { target } => Ok(target.lock().clone()),
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn link(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        match &self.entry {
            TmpfsEntry::Dir { children } => {
                let mut children = children.lock();
                if !children.contains_key(old_name) { return Err(VfsError::NotFound); }
                if children.contains_key(new_name) { return Err(VfsError::AlreadyExists); }
                let inode = children.get(old_name).unwrap().clone();
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
                let entry = if ft == FileType::Fifo { TmpfsEntry::Fifo } else { TmpfsEntry::File { data: Mutex::new(Vec::new()) } };
                let child = Arc::new(TmpfsInode {
                    ino,
                    file_type: ft,
                    entry,
                    mtime: Mutex::new(crate::services::wallclock::now_secs()),
                    mode: Mutex::new(mode & 0o7777),
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
                    mtime: Mutex::new(crate::services::wallclock::now_secs()),
                    mode: Mutex::new(mode & 0o777),
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
