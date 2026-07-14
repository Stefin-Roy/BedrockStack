use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;
use spin::Once;

use super::error::VfsError;
use super::irq::IrqMutex;
use super::mount::DriveMount;

pub struct DriveMap {
    drives: IrqMutex<[Option<Arc<DriveMount>>; 26]>,
    by_id: Once<IrqMutex<HashMap<u64, (char, Arc<DriveMount>)>>>,
}

impl DriveMap {
    pub const fn new() -> Self {
        const NONE: Option<Arc<DriveMount>> = None;
        DriveMap {
            drives: IrqMutex::new([NONE; 26]),
            by_id: Once::new(),
        }
    }

    fn by_id_map(&self) -> &IrqMutex<HashMap<u64, (char, Arc<DriveMount>)>> {
        self.by_id.call_once(|| IrqMutex::new(HashMap::new()))
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
        drives[idx] = Some(mount.clone());
        self.by_id_map().lock().insert(mount.id, (letter, mount));
        Ok(())
    }

    pub fn lookup(&self, letter: char) -> Result<Arc<DriveMount>, VfsError> {
        let idx = Self::letter_index(letter)?;
        let drives = self.drives.lock();
        drives[idx].clone().ok_or(VfsError::NotFound)
    }

    pub fn lookup_by_id(&self, id: u64) -> Option<(char, Arc<DriveMount>)> {
        self.by_id_map().lock().get(&id).cloned()
    }

    pub fn remove(&self, letter: char) -> Result<Arc<DriveMount>, VfsError> {
        let idx = Self::letter_index(letter)?;
        let mut drives = self.drives.lock();
        let mount = drives[idx].take().ok_or(VfsError::NotFound)?;
        self.by_id_map().lock().remove(&mount.id);
        Ok(mount)
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
