use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use super::error::VfsError;
use super::file::FileDescription;

struct FdTableInner {
    fds: Vec<Option<Arc<FileDescription>>>,
    free_list: Vec<u32>,
}

pub struct FdTable {
    inner: Mutex<FdTableInner>,
}

impl FdTable {
    pub const fn new() -> Self {
        FdTable {
            inner: Mutex::new(FdTableInner {
                fds: Vec::new(),
                free_list: Vec::new(),
            }),
        }
    }

    pub fn alloc(&self, fd: FileDescription) -> u32 {
        let mut inner = self.inner.lock();
        if let Some(idx) = inner.free_list.pop() {
            inner.fds[idx as usize] = Some(Arc::new(fd));
            return idx;
        }
        let idx = inner.fds.len() as u32;
        inner.fds.push(Some(Arc::new(fd)));
        idx
    }

    pub fn get(&self, fd: u32) -> Result<Arc<FileDescription>, VfsError> {
        let inner = self.inner.lock();
        inner
            .fds
            .get(fd as usize)
            .and_then(|s| s.as_ref())
            .cloned()
            .ok_or(VfsError::BadFileDescriptor)
    }

    pub fn free(&self, fd: u32) -> Result<(), VfsError> {
        let mut inner = self.inner.lock();
        match inner.fds.get_mut(fd as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                inner.free_list.push(fd);
                Ok(())
            }
            _ => Err(VfsError::BadFileDescriptor),
        }
    }

    pub fn dup(&self, old_fd: u32) -> Result<u32, VfsError> {
        let mut inner = self.inner.lock();
        let entry = inner
            .fds
            .get(old_fd as usize)
            .and_then(|s| s.as_ref())
            .cloned()
            .ok_or(VfsError::BadFileDescriptor)?;
        if let Some(idx) = inner.free_list.pop() {
            inner.fds[idx as usize] = Some(entry);
            return Ok(idx);
        }
        let idx = inner.fds.len() as u32;
        inner.fds.push(Some(entry));
        Ok(idx)
    }

    pub fn dup2(&self, old_fd: u32, new_fd: u32) -> Result<(), VfsError> {
        let mut inner = self.inner.lock();
        let entry = inner
            .fds
            .get(old_fd as usize)
            .and_then(|s| s.as_ref())
            .cloned()
            .ok_or(VfsError::BadFileDescriptor)?;
        if new_fd as usize >= inner.fds.len() {
            inner.fds.resize(new_fd as usize + 1, None);
        } else {
            // If new_fd was previously free-listed, remove it so alloc() doesn't reuse it.
            inner.free_list.retain(|&x| x != new_fd);
        }
        // Overwriting an occupied slot drops the old FileDescription (close semantics).
        inner.fds[new_fd as usize] = Some(entry);
        Ok(())
    }

    pub fn iter_active(&self) -> Vec<Arc<FileDescription>> {
        let inner = self.inner.lock();
        inner
            .fds
            .iter()
            .filter_map(|s| s.as_ref().cloned())
            .collect()
    }
}
