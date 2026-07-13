use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::string::String;

use crate::filesystems::fstypes;

pub mod dentry;
pub mod drive;
pub mod error;
pub mod fdtable;
pub mod file;
pub mod inode;
pub mod irq;
pub mod mount;
pub mod path;
pub mod superblock;
pub mod types;

use dentry::{Dcache, Dentry};
use drive::DriveMap;
use error::VfsError;
use fdtable::FdTable;
use file::FileDescription;
use inode::Inode;
use irq::IrqMutex;
use mount::DriveMount;
use path::{resolve, resolve_parent, walk_from, split_components, next_mount_id};
use superblock::SuperBlock;
use types::{DirEntry, FileType, OpenFlags, SeekFrom, Stat};

static VFS_INIT: AtomicBool = AtomicBool::new(false);
pub static DRIVE_MAP: DriveMap = DriveMap::new();
pub static FD_TABLE: FdTable = FdTable::new();
pub static DCACHE: Dcache = Dcache::new();

/// Initialise the VFS layer. Must be called once during kernel boot.
/// Mounts a tmpfs on drive A> as the system drive.
pub fn init() -> Result<(), VfsError> {
    if VFS_INIT.load(Ordering::SeqCst) {
        return Ok(());
    }
    VFS_INIT.store(true, Ordering::SeqCst);

    fstypes::register_all();

    mount("tmpfs", None, 'A')?;

    mkdir("A>tmp")?;
    mkdir("A>dev")?;

    log::info!("VFS: A> (tmpfs) ready");
    Ok(())
}

/// Mount a filesystem onto a drive letter.
pub fn mount(fstype: &str, device: Option<Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>>, drive: char) -> Result<(), VfsError> {
    let fs = fstypes::lookup(fstype).ok_or(VfsError::NotFound)?;
    let (sb, root_ops) = fs.mount(device.clone())?;

    let root_inode = Arc::new(Inode::new(root_ops));
    root_inode.set_sb(sb.clone());

    let root_dentry = Dentry::new("", Some(root_inode));
    root_dentry.set_mount_point(true);

    let mount = DriveMount::new(next_mount_id(), root_dentry, sb, device);
    DRIVE_MAP.assign(drive, Arc::new(mount))?;

    log::info!("VFS: mounted {} on {}>", fstype, drive);
    Ok(())
}

/// Bind-mount a folder as the root of a new drive letter.
pub fn mount_virtual(source: &str, drive: char) -> Result<(), VfsError> {
    let src_dentry = resolve(source, &DRIVE_MAP, &DCACHE)?;

    let src_inode = {
        let lock = src_dentry.inode.lock();
        lock.as_ref().cloned().ok_or(VfsError::NotFound)?
    };

    let sb = src_inode.sb.get().cloned().ok_or(VfsError::NotFound)?;
    let bind_dentry = Dentry::new("", Some(src_inode));
    bind_dentry.set_mount_point(true);

    let mount = DriveMount::new(next_mount_id(), bind_dentry, sb, None);
    DRIVE_MAP.assign(drive, Arc::new(mount))?;

    log::info!("VFS: bind-mounted {} on {}>", source, drive);
    Ok(())
}

/// Unmount a drive letter.
pub fn unmount(drive: char) -> Result<(), VfsError> {
    DRIVE_MAP.remove(drive)?;
    log::info!("VFS: unmounted {}>", drive);
    Ok(())
}

/// Open or create a file.
pub fn open(path: &str, flags: OpenFlags) -> Result<u32, VfsError> {
    let (letter, inner) = path::split_drive_path(path)?;
    let mount = DRIVE_MAP.lookup(letter)?;

    let create = flags.contains(OpenFlags::CREATE);
    let trunc = flags.contains(OpenFlags::TRUNC);

    let components = split_components(inner);
    if components.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    let leaf_name = String::from(*components.last().unwrap());
    let parent_components = &components[..components.len() - 1];

    let parent = if parent_components.is_empty() {
        mount.root.clone()
    } else {
        walk_from(mount.root.clone(), parent_components, &DCACHE)?
    };

    {
        let inode_lock = parent.inode.lock();
        let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
        if parent_inode.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
    }

    let existing = {
        let inode_lock = parent.inode.lock();
        inode_lock.as_ref().and_then(|p| p.ops.lookup(&leaf_name).ok())
    };

    let inode: Arc<Inode> = match existing {
        Some(child_ops) => {
            let inode = Arc::new(Inode::new(child_ops));

            let sb = parent.inode.lock()
                .as_ref()
                .and_then(|i| i.sb.get().cloned());

            if let Some(sb) = sb {
                inode.set_sb(sb);
            }

            if trunc {
                inode.ops.truncate(0)?;
                inode.size.store(0, Ordering::Relaxed);
            }

            let children = parent.children.lock();
            let child_dentry = children.iter().find(|c| c.name == leaf_name).cloned();
            drop(children);

            if let Some(cd) = child_dentry {
                *cd.inode.lock() = Some(inode.clone());
            }

            inode
        }
        None => {
            if !create {
                return Err(VfsError::NotFound);
            }
            let child_ops = {
                let inode_lock = parent.inode.lock();
                let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
                parent_inode.ops.create(&leaf_name)?
            };
            let sb = parent.inode.lock()
                .as_ref()
                .and_then(|i| i.sb.get().cloned())
                .ok_or(VfsError::NotFound)?;

            let inode = Arc::new(Inode::new(child_ops));
            inode.set_sb(sb);

            let child_dentry = Dentry::new(&leaf_name, Some(inode.clone()));
            parent.children.lock().push(child_dentry.clone());
            let parent_ino = {
                let inode_lock = parent.inode.lock();
                inode_lock.as_ref().map(|i| i.ino).unwrap_or(0)
            };
            DCACHE.insert(parent_ino, leaf_name.clone(), Arc::downgrade(&child_dentry));

            inode
        }
    };

    let fd_dentry = {
        let children = parent.children.lock();
        children.iter().find(|c| c.name == leaf_name).cloned().ok_or(VfsError::NotFound)?
    };

    let fd = FileDescription::new(fd_dentry, inode.clone(), flags);
    Ok(FD_TABLE.alloc(fd))
}

/// Close a file descriptor.
pub fn close(fd: u32) -> Result<(), VfsError> {
    FD_TABLE.free(fd)
}

/// Read from a file descriptor.
pub fn read(fd: u32, buf: &mut [u8]) -> Result<usize, VfsError> {
    let file = FD_TABLE.get(fd)?;
    let offset = {
        let mut pos = file.pos.lock();
        let cur = *pos;
        let count = file.inode.ops.read_at(cur, buf)?;
        *pos = cur + count as u64;
        count
    };
    Ok(offset)
}

/// Write to a file descriptor.
pub fn write(fd: u32, buf: &[u8]) -> Result<usize, VfsError> {
    let file = FD_TABLE.get(fd)?;
    let count = {
        let mut pos = file.pos.lock();
        let cur = *pos;
        let count = file.inode.ops.write_at(cur, buf)?;
        let new_size = cur + count as u64;
        if new_size > file.inode.size.load(Ordering::Relaxed) {
            file.inode.size.store(new_size, Ordering::Relaxed);
        }
        *pos = new_size;
        count
    };
    Ok(count)
}

/// Seek to a position in a file descriptor.
pub fn seek(fd: u32, offset: i64, whence: SeekFrom) -> Result<u64, VfsError> {
    let file = FD_TABLE.get(fd)?;
    let mut pos = file.pos.lock();
    let size = file.inode.size.load(Ordering::Relaxed);
    let new_pos = match whence {
        SeekFrom::Start(o) => o as i64,
        SeekFrom::Current(o) => (*pos as i64).checked_add(o).ok_or(VfsError::InvalidInput)?,
        SeekFrom::End(o) => (size as i64).checked_add(o).ok_or(VfsError::InvalidInput)?,
    };
    if new_pos < 0 {
        return Err(VfsError::InvalidInput);
    }
    *pos = new_pos as u64;
    Ok(*pos)
}

/// Create a directory.
pub fn mkdir(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path, &DRIVE_MAP, &DCACHE)?;
    let inode_lock = parent.inode.lock();
    let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    if parent_inode.file_type != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    let child_ops = parent_inode.ops.mkdir(&name)?;
    let sb = parent_inode.sb.get().cloned().ok_or(VfsError::NotFound)?;
    drop(inode_lock);

    let child_inode = Arc::new(Inode::new(child_ops));
    child_inode.set_sb(sb);

    let child = Dentry::new(&name, Some(child_inode));
    parent.children.lock().push(child.clone());

    let parent_ino = {
        let inode_lock = parent.inode.lock();
        inode_lock.as_ref().map(|i| i.ino).unwrap_or(0)
    };
    DCACHE.insert(parent_ino, name, Arc::downgrade(&child));
    Ok(())
}

/// Remove an empty directory.
pub fn rmdir(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path, &DRIVE_MAP, &DCACHE)?;
    let parent_ino = {
        let inode_lock = parent.inode.lock();
        inode_lock.as_ref().map(|i| i.ino).ok_or(VfsError::NotFound)?
    };

    {
        let mut children = parent.children.lock();
        if let Some(idx) = children.iter().position(|c| c.name == name) {
            let child = children.remove(idx);
            child.inode.lock().take();
        }
    }

    let inode_lock = parent.inode.lock();
    let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    parent_inode.ops.rmdir(&name)?;
    drop(inode_lock);

    DCACHE.evict(parent_ino, &name);
    Ok(())
}

/// List directory contents.
pub fn readdir(path: &str) -> Result<Vec<DirEntry>, VfsError> {
    let dentry = resolve(path, &DRIVE_MAP, &DCACHE)?;
    let inode_lock = dentry.inode.lock();
    let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    if inode.file_type != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    inode.ops.readdir()
}

/// Unlink (delete) a file.
pub fn unlink(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path, &DRIVE_MAP, &DCACHE)?;
    let parent_ino = {
        let inode_lock = parent.inode.lock();
        inode_lock.as_ref().map(|i| i.ino).ok_or(VfsError::NotFound)?
    };

    {
        let mut children = parent.children.lock();
        if let Some(idx) = children.iter().position(|c| c.name == name) {
            let child = children.remove(idx);
            child.inode.lock().take();
        }
    }

    let inode_lock = parent.inode.lock();
    let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    parent_inode.ops.unlink(&name)?;
    drop(inode_lock);

    DCACHE.evict(parent_ino, &name);
    Ok(())
}

/// Rename (or move) a file or directory. Supports cross-drive moves for regular files.
pub fn rename(old_path: &str, new_path: &str) -> Result<(), VfsError> {
    let (old_parent, old_name) = resolve_parent(old_path, &DRIVE_MAP, &DCACHE)?;
    let (new_parent, new_name) = resolve_parent(new_path, &DRIVE_MAP, &DCACHE)?;

    {
        let lock = old_parent.inode.lock();
        let p = lock.as_ref().ok_or(VfsError::NotFound)?;
        if p.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
    }
    {
        let lock = new_parent.inode.lock();
        let p = lock.as_ref().ok_or(VfsError::NotFound)?;
        if p.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
    }

    let same_parent = Arc::ptr_eq(&old_parent, &new_parent);

    let (old_ino, old_ops, new_ops);
    {
        let lock = old_parent.inode.lock();
        let p = lock.as_ref().ok_or(VfsError::NotFound)?;
        old_ino = p.ino;
        old_ops = p.ops.clone();
    }
    {
        let lock = new_parent.inode.lock();
        let p = lock.as_ref().ok_or(VfsError::NotFound)?;
        new_ops = p.ops.clone();
    }

    if same_parent {
        old_ops.rename(&old_name, &new_name)?;
        let mut children = old_parent.children.lock();
        if let Some(child) = children.iter_mut().find(|c| c.name == old_name) {
            child.name = new_name.clone();
        }
        drop(children);
        DCACHE.evict(old_ino, &old_name);
    } else {
        let child_ops = old_ops.lookup(&old_name)?;
        if child_ops.file_type() == FileType::Directory {
            return Err(VfsError::CrossDeviceLink);
        }
        let size = child_ops.size();
        let mut buf = alloc::vec![0u8; size as usize];
        if size > 0 {
            child_ops.read_at(0, &mut buf)?;
        }
        let new_child_ops = new_ops.create(&new_name)?;
        if size > 0 {
            new_child_ops.write_at(0, &buf)?;
        }
        old_ops.unlink(&old_name)?;

        let moved = {
            let mut children = old_parent.children.lock();
            children.iter().position(|c| c.name == old_name)
                .map(|idx| children.remove(idx))
        };
        if let Some(child) = moved {
            child.name = new_name.clone();
            *child.parent.lock() = Arc::downgrade(&new_parent);
            let new_ino = new_parent.inode.lock()
                .as_ref().map(|i| i.ino).unwrap_or(0);
            DCACHE.insert(new_ino, new_name.clone(), Arc::downgrade(&child));
            new_parent.children.lock().push(child);
        }
        DCACHE.evict(old_ino, &old_name);
    }

    Ok(())
}

/// Get file metadata by path.
pub fn stat(path: &str) -> Result<Stat, VfsError> {
    let dentry = resolve(path, &DRIVE_MAP, &DCACHE)?;
    let inode_lock = dentry.inode.lock();
    let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    inode.ops.getattr()
}
