use alloc::sync::Arc;
use spin::Mutex;

use crate::filesystems::fstypes;

pub mod dentry;
pub mod error;
pub mod fdtable;
pub mod file;
pub mod inode;
pub mod irq;
pub mod mount;
pub mod superblock;
pub mod types;

use dentry::Dentry;
use error::VfsError;
use inode::Inode;
use mount::DriveMount;

// ---------------------------------------------------------------------------
// Mount registry — stores mounted filesystems by drive letter for direct access.
// ---------------------------------------------------------------------------

static MOUNT_REGISTRY: Mutex<[Option<Arc<DriveMount>>; 26]> = Mutex::new([const { None }; 26]);

fn drive_index(drive: char) -> Option<usize> {
    let c = drive.to_ascii_uppercase();
    if c >= 'A' && c <= 'Z' {
        Some((c as u8 - b'A') as usize)
    } else {
        None
    }
}

/// Register a mounted filesystem under its drive letter.
pub fn register_mount(drive: char, mount: Arc<DriveMount>) {
    if let Some(idx) = drive_index(drive) {
        MOUNT_REGISTRY.lock()[idx] = Some(mount);
    }
}

/// Look up a mounted filesystem by drive letter.
pub fn get_mount(drive: char) -> Option<Arc<DriveMount>> {
    drive_index(drive).and_then(|idx| MOUNT_REGISTRY.lock()[idx].clone())
}

// ---------------------------------------------------------------------------
// Mount / drive management (internal — called by partition/mod.rs and mount caps)
// ---------------------------------------------------------------------------

pub fn mount(
    fstype: &str,
    device: Option<Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>>,
    drive: char,
) -> Result<Arc<DriveMount>, VfsError> {
    let fs = fstypes::lookup(fstype).ok_or(VfsError::NotFound)?;
    let (sb, root_ops) = fs.mount(device.clone())?;
    let root_inode = Arc::new(Inode::new(root_ops));
    let root_dentry = Dentry::new("", Some(root_inode));
    root_dentry.set_mount_point(true);

    let mount = Arc::new(DriveMount::new(mount::next_mount_id(), root_dentry, sb, device));
    register_mount(drive, mount.clone());
    log::info!("VFS: mounted {} on {}>", fstype, drive);
    Ok(mount)
}
