use alloc::sync::Arc;

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
    log::info!("VFS: mounted {} on {}>", fstype, drive);
    Ok(mount)
}
