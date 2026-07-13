use alloc::sync::Arc;

use super::error::VfsError;
use super::irq::IrqMutex;
use super::mount::DriveMount;

pub struct DriveMap {
    drives: IrqMutex<[Option<Arc<DriveMount>>; 26]>,
}

impl DriveMap {
    pub const fn new() -> Self {
        const NONE: Option<Arc<DriveMount>> = None;
        DriveMap {
            drives: IrqMutex::new([NONE; 26]),
        }
    }

    fn letter_index(letter: char) -> Result<usize, VfsError> {
        let idx = letter.to_ascii_uppercase() as usize;
        if idx < 65 || idx > 90 {
            return Err(VfsError::InvalidInput);
        }
        Ok(idx - 65)
    }

    pub fn assign(&self, letter: char, mount: Arc<DriveMount>) -> Result<(), VfsError> {
        let idx = Self::letter_index(letter)?;
        let mut drives = self.drives.lock();
        if drives[idx].is_some() {
            return Err(VfsError::AlreadyExists);
        }
        drives[idx] = Some(mount);
        Ok(())
    }

    pub fn lookup(&self, letter: char) -> Result<Arc<DriveMount>, VfsError> {
        let idx = Self::letter_index(letter)?;
        let drives = self.drives.lock();
        drives[idx].clone().ok_or(VfsError::NotFound)
    }

    pub fn remove(&self, letter: char) -> Result<Arc<DriveMount>, VfsError> {
        let idx = Self::letter_index(letter)?;
        let mut drives = self.drives.lock();
        drives[idx].take().ok_or(VfsError::NotFound)
    }

    pub fn iter(&self) -> impl Iterator<Item = (char, Arc<DriveMount>)> {
        let drives = self.drives.lock();
        let snapshot: Vec<_> = drives
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.as_ref().map(|m| ((i as u8 + 65) as char, m.clone())))
            .collect();
        snapshot.into_iter()
    }
}
