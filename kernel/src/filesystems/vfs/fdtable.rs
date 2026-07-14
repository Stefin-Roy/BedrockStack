use alloc::sync::Arc;
use alloc::vec::Vec;

use super::error::VfsError;
use super::file::FileDescription;
use super::irq::IrqMutex;

pub struct FdTable {
    fds: IrqMutex<Vec<Option<Arc<FileDescription>>>>,
}

impl FdTable {
    pub const fn new() -> Self {
        FdTable {
            fds: IrqMutex::new(Vec::new()),
        }
    }

    pub fn alloc(&self, fd: FileDescription) -> u32 {
        let mut fds = self.fds.lock();
        for (i, slot) in fds.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(Arc::new(fd));
                return i as u32;
            }
        }
        let idx = fds.len();
        fds.push(Some(Arc::new(fd)));
        idx as u32
    }

    pub fn get(&self, fd: u32) -> Result<Arc<FileDescription>, VfsError> {
        let fds = self.fds.lock();
        fds.get(fd as usize)
            .and_then(|s| s.as_ref())
            .cloned()
            .ok_or(VfsError::BadFileDescriptor)
    }

    pub fn free(&self, fd: u32) -> Result<(), VfsError> {
        let mut fds = self.fds.lock();
        match fds.get_mut(fd as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(VfsError::BadFileDescriptor),
        }
    }

    pub fn dup(&self, old_fd: u32) -> Result<u32, VfsError> {
        let entry = {
            let fds = self.fds.lock();
            fds.get(old_fd as usize)
                .and_then(|s| s.as_ref())
                .cloned()
                .ok_or(VfsError::BadFileDescriptor)?
        };
        let mut fds = self.fds.lock();
        for (i, slot) in fds.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(entry);
                return Ok(i as u32);
            }
        }
        let idx = fds.len();
        fds.push(Some(entry));
        Ok(idx as u32)
    }

    pub fn dup2(&self, old_fd: u32, new_fd: u32) -> Result<(), VfsError> {
        let entry = {
            let fds = self.fds.lock();
            fds.get(old_fd as usize)
                .and_then(|s| s.as_ref())
                .cloned()
                .ok_or(VfsError::BadFileDescriptor)?
        };
        let mut fds = self.fds.lock();
        if new_fd as usize >= fds.len() {
            fds.resize(new_fd as usize + 1, None);
        }
        fds[new_fd as usize] = Some(entry);
        Ok(())
    }

    pub fn iter_active(&self) -> Vec<Arc<FileDescription>> {
        let fds = self.fds.lock();
        fds.iter().filter_map(|s| s.as_ref().cloned()).collect()
    }
}
