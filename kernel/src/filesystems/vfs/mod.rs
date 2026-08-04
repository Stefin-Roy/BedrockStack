use alloc::sync::Arc;

use crate::filesystems::fstypes;

pub mod dentry;
pub mod drive;
pub mod error;
pub mod fdtable;
pub mod file;
pub mod inode;
pub mod irq;
pub mod mount;
pub mod superblock;
pub mod types;

use dentry::Dentry;
use drive::DriveMap;
use error::VfsError;
use inode::Inode;
use mount::DriveMount;

pub static DRIVE_MAP: DriveMap = DriveMap::new();

// ---------------------------------------------------------------------------
// Mount / drive management (internal — called by partition/mod.rs and mount caps)
// ---------------------------------------------------------------------------

pub fn mount(
    fstype: &str,
    device: Option<Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>>,
    drive: char,
) -> Result<(), VfsError> {
    let fs = fstypes::lookup(fstype).ok_or(VfsError::NotFound)?;
    let (sb, root_ops) = fs.mount(device.clone())?;
    let root_inode = Arc::new(Inode::new(root_ops));
    let root_dentry = Dentry::new("", Some(root_inode));
    root_dentry.set_mount_point(true);

    let mount = DriveMount::new(mount::next_mount_id(), root_dentry, sb, device);
    DRIVE_MAP.assign(drive, Arc::new(mount))?;
    log::info!("VFS: mounted {} on {}>", fstype, drive);
    Ok(())
}

/// Check whether a dentry is in the tree rooted at `mount_root`.
fn dentry_belongs_to_mount(dentry: &Arc<Dentry>, mount_root: &Arc<Dentry>) -> bool {
    let target_mid = mount_root.get_mount_id();
    target_mid != 0 && dentry.get_mount_id() == target_mid
}
