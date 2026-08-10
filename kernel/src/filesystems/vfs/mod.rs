use alloc::sync::Arc;
use spin::Mutex;

use crate::filesystems::fstypes;

pub mod dentry;
pub mod error;
pub mod fdtable;
pub mod file;
pub mod file_ops;
pub mod inode;
pub mod irq;
pub mod mount;
pub mod superblock;
pub mod synthetic;
pub mod types;

use dentry::Dentry;
use error::VfsError;
use inode::Inode;
use mount::DriveMount;

// ---------------------------------------------------------------------------
// Mount registry — stores mounted filesystems by name (e.g. "A", "esp") for
// direct access and for the synthetic `/mnt` tree.  A mount point registered
// under a name appears as `/mnt/<name>`.
// ---------------------------------------------------------------------------

static MOUNT_REGISTRY: Mutex<alloc::vec::Vec<(alloc::string::String, Arc<DriveMount>)>> =
    Mutex::new(alloc::vec::Vec::new());

/// Register a mounted filesystem under `name`.  Re-registering an existing
/// name replaces the previous mount (the old `DriveMount` drops when the last
/// handle does).
pub fn register_mount(name: &str, mount: Arc<DriveMount>) {
    let mut reg = MOUNT_REGISTRY.lock();
    if let Some(slot) = reg.iter_mut().find(|(n, _)| n == name) {
        slot.1 = mount;
    } else {
        reg.push((alloc::string::String::from(name), mount));
    }
}

/// Look up a mounted filesystem by name.
pub fn get_mount(name: &str) -> Option<Arc<DriveMount>> {
    MOUNT_REGISTRY
        .lock()
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, m)| m.clone())
}

/// Enumerate the currently-registered mount names, sorted, so the synthetic
/// `/mnt` tree renders stable directory order.
pub fn list_mounts() -> alloc::vec::Vec<alloc::string::String> {
    let reg = MOUNT_REGISTRY.lock();
    let mut names: alloc::vec::Vec<alloc::string::String> =
        reg.iter().map(|(n, _)| n.clone()).collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Mount / drive management (internal — called by partition/mod.rs and mount
// paths).  `name` is the mount-point name shown under /mnt (e.g. "A", "esp").
// ---------------------------------------------------------------------------

pub fn mount(
    fstype: &str,
    device: Option<Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>>,
    name: &str,
) -> Result<Arc<DriveMount>, VfsError> {
    let fs = fstypes::lookup(fstype).ok_or(VfsError::NotFound)?;
    let (sb, root_ops) = fs.mount(device.clone())?;
    let root_inode = Arc::new(Inode::new(root_ops));
    let root_dentry = Dentry::new("", Some(root_inode));
    root_dentry.set_mount_point(true);

    let mount = Arc::new(DriveMount::new(mount::next_mount_id(), root_dentry, sb, device));
    register_mount(name, mount.clone());
    log::info!("VFS: mounted {} at /mnt/{}", fstype, name);
    Ok(mount)
}
