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

use dentry::{Dentry, dcache};
use drive::DriveMap;
use error::VfsError;
use fdtable::FdTable;
use file::FileDescription;
use inode::Inode;
use mount::DriveMount;
use superblock::StatFs;
use types::{DirEntry, FileType, OpenFlags, SeekFrom, Stat};

static VFS_INIT: AtomicBool = AtomicBool::new(false);
pub static DRIVE_MAP: DriveMap = DriveMap::new();
pub static FD_TABLE: FdTable = FdTable::new();

pub struct CurrentWorkingDirectory {
    pub drive: char,
    pub dentry: Arc<Dentry>,
}

pub static CWD: irq::IrqMutex<Option<CurrentWorkingDirectory>> = irq::IrqMutex::new(None);

// ---------------------------------------------------------------------------
// Path resolution (supports absolute X>path and relative paths via CWD)
// ---------------------------------------------------------------------------

/// Resolve a path to its drive letter and target dentry.
/// Absolute: `X>rest/of/path`. Relative: resolved against CWD.
pub fn resolve_path(path: &str) -> Result<(char, Arc<Dentry>), VfsError> {
    if path.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    if let Ok((letter, inner)) = path::split_drive_path(path) {
        let mount = DRIVE_MAP.lookup(letter)?;
        if inner.is_empty() {
            return Ok((letter, mount.root.clone()));
        }
        let components = path::split_components(inner);
        let dentry = path::walk_from(mount.root.clone(), &components)?;
        Ok((letter, dentry))
    } else {
        let cwd = CWD.lock();
        let cwd = cwd.as_ref().ok_or(VfsError::NotFound)?;
        let components = path::split_components(path);
        let dentry = path::walk_from(cwd.dentry.clone(), &components)?;
        Ok((cwd.drive, dentry))
    }
}

/// Resolve parent dentry + leaf name from a path.
fn resolve_parent(path: &str) -> Result<(Arc<Dentry>, String), VfsError> {
    let (start_dentry, inner) = if let Ok((letter, inner)) = path::split_drive_path(path) {
        let mount = DRIVE_MAP.lookup(letter)?;
        (mount.root.clone(), inner)
    } else {
        let cwd = CWD.lock();
        let cwd = cwd.as_ref().ok_or(VfsError::NotFound)?;
        (cwd.dentry.clone(), path)
    };

    let components = path::split_components(inner);
    if components.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    let leaf_name = String::from(*components.last().unwrap());
    let parent_components = &components[..components.len() - 1];

    let parent = if parent_components.is_empty() {
        start_dentry
    } else {
        path::walk_from(start_dentry, parent_components)?
    };

    Ok((parent, leaf_name))
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

pub fn init() -> Result<(), VfsError> {
    if VFS_INIT.load(Ordering::SeqCst) {
        return Ok(());
    }

    fstypes::register_all();
    mount("tmpfs", None, 'A')?;
    mkdir("A>tmp")?;

    // Standard FDs 0/1/2 (placeholder files — replaced by real console later)
    {
        let _fd0 = open("A>tmp/stdin", OpenFlags::CREATE | OpenFlags::READ)?;
        let _fd1 = open("A>tmp/stdout", OpenFlags::CREATE | OpenFlags::WRITE)?;
        let _fd2 = open("A>tmp/stderr", OpenFlags::CREATE | OpenFlags::WRITE)?;
        debug_assert!(_fd0 == 0 && _fd1 == 1 && _fd2 == 2);
    }

    // Set CWD to A> root
    let root = DRIVE_MAP.lookup('A')?.root.clone();
    *CWD.lock() = Some(CurrentWorkingDirectory { drive: 'A', dentry: root });

    VFS_INIT.store(true, Ordering::SeqCst);
    log::info!("VFS: A> (tmpfs) ready");
    Ok(())
}

// ---------------------------------------------------------------------------
// Mount / drive management
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

    let mount = DriveMount::new(path::next_mount_id(), root_dentry, sb, device);
    DRIVE_MAP.assign(drive, Arc::new(mount))?;
    log::info!("VFS: mounted {} on {}>", fstype, drive);
    Ok(())
}

pub fn mount_virtual(source: &str, drive: char) -> Result<(), VfsError> {
    let (letter, src_dentry) = resolve_path(source)?;
    let src_inode = {
        let lock = src_dentry.inode.lock();
        lock.as_ref().cloned().ok_or(VfsError::NotFound)?
    };

    let src_mount = DRIVE_MAP.lookup(letter)?;
    let sb = src_mount.sb.clone();

    let bind_dentry = Dentry::new("", Some(src_inode));
    bind_dentry.set_mount_point(true);

    let mount = DriveMount::new(path::next_mount_id(), bind_dentry, sb, None);
    DRIVE_MAP.assign(drive, Arc::new(mount))?;
    log::info!("VFS: bind-mounted {} on {}>", source, drive);
    Ok(())
}

pub fn mount_at(
    fstype: &str,
    device: Option<Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>>,
    target_path: &str,
    drive: char,
) -> Result<(), VfsError> {
    // Resolve and verify the target mount point
    let (_, target) = resolve_path(target_path)?;
    {
        let lock = target.inode.lock();
        let inode = lock.as_ref().ok_or(VfsError::NotFound)?;
        if inode.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
    }
    if target.get_mount_id() != 0 {
        return Err(VfsError::AlreadyExists);
    }

    // Mount the filesystem
    let fs = fstypes::lookup(fstype).ok_or(VfsError::NotFound)?;
    let (sb, root_ops) = fs.mount(device.clone())?;
    let root_inode = Arc::new(Inode::new(root_ops));
    let root_dentry = Dentry::new("", Some(root_inode));
    root_dentry.set_mount_point(true);

    let mid = path::next_mount_id();
    let mount = DriveMount::new(mid, root_dentry, sb, device);
    *mount.covered.lock() = Some(Arc::downgrade(&target));
    let mount = Arc::new(mount);

    DRIVE_MAP.assign(drive, mount)?;
    target.set_mount_id(mid);
    log::info!("VFS: mounted {} on {}> (at {})", fstype, drive, target_path);
    Ok(())
}

pub fn unmount(drive: char) -> Result<(), VfsError> {
    // Check CWD not on this drive
    {
        let cwd = CWD.lock();
        if let Some(ref cwd) = *cwd {
            if cwd.drive == drive {
                return Err(VfsError::MountBusy);
            }
        }
    }
    // Flush FS data (FAT cache, FSInfo, dirty bit) before unmount
    sync_drive(drive)?;

    // Check no open FDs reference this drive
    let mount = DRIVE_MAP.lookup(drive)?;
    for fd in FD_TABLE.iter_active() {
        if dentry_belongs_to_mount(&fd.dentry, &mount.root) {
            return Err(VfsError::MountBusy);
        }
    }

    // Clear the covered dentry's mount_id before removal
    if let Some(weak) = mount.covered.lock().take() {
        if let Some(d) = weak.upgrade() {
            d.set_mount_id(0);
            d.set_mount_point(false);
        }
    }

    DRIVE_MAP.remove(drive)?;
    log::info!("VFS: unmounted {}>", drive);
    Ok(())
}

/// Check whether a dentry is in the tree rooted at `mount_root`.
fn dentry_belongs_to_mount(dentry: &Arc<Dentry>, mount_root: &Arc<Dentry>) -> bool {
    if Arc::ptr_eq(dentry, mount_root) {
        return true;
    }
    // Walk up to the root
    let mut current = dentry.clone();
    loop {
        let parent = current.parent.lock().upgrade();
        match parent {
            Some(p) => {
                if Arc::ptr_eq(&p, mount_root) {
                    return true;
                }
                current = p;
            }
            None => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// CWD
// ---------------------------------------------------------------------------

pub fn chdir(path: &str) -> Result<(), VfsError> {
    let (letter, dentry) = resolve_path(path)?;
    {
        let inode_lock = dentry.inode.lock();
        let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
        if inode.file_type != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
    }
    let mut cwd = CWD.lock();
    *cwd = Some(CurrentWorkingDirectory { drive: letter, dentry });
    Ok(())
}

pub fn getcwd() -> Result<String, VfsError> {
    let cwd = CWD.lock();
    let cwd = cwd.as_ref().ok_or(VfsError::NotFound)?;
    let mut parts: Vec<String> = Vec::new();
    let mut current = cwd.dentry.clone();
    loop {
        let name = current.name.lock().clone();
        if name.is_empty() {
            break;
        }
        parts.push(name);
        let parent = current.parent.lock().upgrade();
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }
    parts.reverse();
    let mut result = String::from(cwd.drive);
    result.push('>');
    if parts.is_empty() {
        // Root of drive
    } else {
        result.push_str(&parts.join("/"));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

pub fn open(path: &str, flags: OpenFlags) -> Result<u32, VfsError> {
    let create = flags.contains(OpenFlags::CREATE);
    let trunc = flags.contains(OpenFlags::TRUNC);

    let (parent, leaf_name) = resolve_parent(path)?;

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

    // O_EXCL: fail if file already exists
    if flags.contains(OpenFlags::EXCL) && existing.is_some() {
        return Err(VfsError::AlreadyExists);
    }

    let inode: Arc<Inode> = match existing {
        Some(child_ops) => {
            let inode = Arc::new(Inode::new(child_ops));
            if trunc {
                inode.ops.truncate(0)?;
                inode.size.store(0, Ordering::Relaxed);
            }
            // Update cached dentry
            if let Some(cd) = parent.children.lock().get(&leaf_name) {
                *cd.inode.lock() = Some(inode.clone());
            }
            inode
        }
        None => {
            if !create {
                return Err(VfsError::NotFound);
            }
            let child_ops = {
                let lock = parent.inode.lock();
                let p = lock.as_ref().ok_or(VfsError::NotFound)?;
                p.ops.create(&leaf_name)?
            };
            let inode = Arc::new(Inode::new(child_ops));
            let child_dentry = Dentry::new(&leaf_name, Some(inode.clone()));
            *child_dentry.parent.lock() = Arc::downgrade(&parent);
            parent.children.lock().insert(leaf_name.clone(), child_dentry.clone());
            let parent_ino = parent.inode.lock()
                .as_ref().map(|i| i.ino).unwrap_or(0);
            dcache().insert(parent_ino, leaf_name.clone(), Arc::downgrade(&child_dentry));
            inode
        }
    };

    let fd_dentry = parent.children.lock()
        .get(&leaf_name)
        .cloned()
        .ok_or(VfsError::NotFound)?;

    let fd = FileDescription::new(fd_dentry, inode, flags);
    Ok(FD_TABLE.alloc(fd))
}

pub fn close(fd: u32) -> Result<(), VfsError> {
    FD_TABLE.free(fd)
}

pub fn read(fd: u32, buf: &mut [u8]) -> Result<usize, VfsError> {
    let file = FD_TABLE.get(fd)?;
    if !file.flags.contains(OpenFlags::READ) {
        return Err(VfsError::BadFileDescriptor);
    }
    let result = {
        let mut pos = file.pos.lock();
        let cur = *pos;
        let count = file.inode.ops.read_at(cur, buf)?;
        *pos = cur + count as u64;
        count
    };
    Ok(result)
}

pub fn write(fd: u32, buf: &[u8]) -> Result<usize, VfsError> {
    let file = FD_TABLE.get(fd)?;
    if !file.flags.contains(OpenFlags::WRITE) {
        return Err(VfsError::BadFileDescriptor);
    }
    let result = {
        let mut pos = file.pos.lock();
        let _append_guard = if file.flags.contains(OpenFlags::APPEND) {
            Some(file.inode.append_lock.lock())
        } else {
            None
        };
        // APPEND: serialize read-size + write_at (uses ops.size() to read the
        // authoritative FS size, not the VFS-level cached size)
        let cur = if file.flags.contains(OpenFlags::APPEND) {
            file.inode.ops.size()
        } else {
            *pos
        };
        *pos = cur;
        let count = file.inode.ops.write_at(cur, buf)?;
        let new_size = cur + count as u64;
        if new_size > file.inode.size.load(Ordering::Relaxed) {
            file.inode.size.store(new_size, Ordering::Relaxed);
        }
        *pos = new_size;
        count
    };
    Ok(result)
}

pub fn seek(fd: u32, whence: SeekFrom) -> Result<u64, VfsError> {
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

// ---------------------------------------------------------------------------
// Directory operations
// ---------------------------------------------------------------------------

pub fn mkdir(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path)?;
    let inode_lock = parent.inode.lock();
    let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    if parent_inode.file_type != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    let child_ops = parent_inode.ops.mkdir(&name)?;
    drop(inode_lock);

    let child_inode = Arc::new(Inode::new(child_ops));
    let child = Dentry::new(&name, Some(child_inode));
    *child.parent.lock() = Arc::downgrade(&parent);
    parent.children.lock().insert(name.clone(), child.clone());

    let parent_ino = parent.inode.lock()
        .as_ref().map(|i| i.ino).unwrap_or(0);
    dcache().insert(parent_ino, name, Arc::downgrade(&child));
    Ok(())
}

pub fn rmdir(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path)?;
    let parent_ino = parent.inode.lock()
        .as_ref().map(|i| i.ino).ok_or(VfsError::NotFound)?;

    if let Some(child) = parent.children.lock().remove(&name) {
        child.inode.lock().take();
    }

    let parent_inode = parent.inode.lock();
    let p = parent_inode.as_ref().ok_or(VfsError::NotFound)?;
    p.ops.rmdir(&name)?;
    drop(parent_inode);

    dcache().evict(parent_ino, &name);
    Ok(())
}

pub fn readdir(path: &str) -> Result<Vec<DirEntry>, VfsError> {
    let (_, dentry) = resolve_path(path)?;
    let inode_lock = dentry.inode.lock();
    let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    if inode.file_type != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }

    let mut entries = inode.ops.readdir()?;

    // Prepend . and ..
    let parent_ino = dentry.parent.lock()
        .upgrade()
        .and_then(|p| p.inode.lock().as_ref().map(|i| i.ino))
        .unwrap_or(inode.ino);

    entries.insert(0, DirEntry {
        ino: parent_ino,
        name: String::from(".."),
        file_type: FileType::Directory,
    });
    entries.insert(0, DirEntry {
        ino: inode.ino,
        name: String::from("."),
        file_type: FileType::Directory,
    });

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Namespace operations
// ---------------------------------------------------------------------------

pub fn unlink(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path)?;
    let parent_ino = parent.inode.lock()
        .as_ref().map(|i| i.ino).ok_or(VfsError::NotFound)?;

    // Reject unlinking directories (use rmdir instead)
    if let Some(child) = parent.children.lock().get(&name) {
        let guard = child.inode.lock();
        if let Some(inode) = guard.as_ref() {
            if inode.file_type == FileType::Directory {
                return Err(VfsError::IsADirectory);
            }
        }
    } else {
        let lock = parent.inode.lock();
        let p = lock.as_ref().ok_or(VfsError::NotFound)?;
        if let Ok(child_ops) = p.ops.lookup(&name) {
            if child_ops.file_type() == FileType::Directory {
                return Err(VfsError::IsADirectory);
            }
        }
    }

    if let Some(child) = parent.children.lock().remove(&name) {
        child.inode.lock().take();
    }

    let parent_inode = parent.inode.lock();
    let p = parent_inode.as_ref().ok_or(VfsError::NotFound)?;
    p.ops.unlink(&name)?;
    drop(parent_inode);

    dcache().evict(parent_ino, &name);
    Ok(())
}

pub fn rename(old_path: &str, new_path: &str) -> Result<(), VfsError> {
    let (old_parent, old_name) = resolve_parent(old_path)?;
    let (new_parent, new_name) = resolve_parent(new_path)?;

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

    let (old_ino, old_ops, new_ops) = if same_parent {
        // `old_parent` and `new_parent` are the same Arc here.  Locking
        // their inode fields separately would try to re-lock a non-reentrant
        // spin mutex and deadlock the BSP.
        let inode = old_parent.inode.lock();
        let inode = inode.as_ref().ok_or(VfsError::NotFound)?;
        (inode.ino, inode.ops.clone(), inode.ops.clone())
    } else {
        let o = old_parent.inode.lock();
        let n = new_parent.inode.lock();
        (
            o.as_ref().map(|i| i.ino).unwrap_or(0),
            o.as_ref().ok_or(VfsError::NotFound)?.ops.clone(),
            n.as_ref().ok_or(VfsError::NotFound)?.ops.clone(),
        )
    };

    if same_parent {
        old_ops.rename(&old_name, &new_name)?;
        let mut children = old_parent.children.lock();
        if let Some(child) = children.remove(&old_name) {
            *child.name.lock() = new_name.clone();
            children.insert(new_name.clone(), child.clone());
            drop(children);
            dcache().evict(old_ino, &old_name);
            dcache().insert(old_ino, new_name, Arc::downgrade(&child));
        } else {
            drop(children);
            dcache().evict(old_ino, &old_name);
        }
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
            if new_child_ops.write_at(0, &buf).is_err() {
                let _ = new_ops.unlink(&new_name);
                return Err(VfsError::IOError);
            }
        }
        old_ops.unlink(&old_name).map_err(|e| {
            let _ = new_ops.unlink(&new_name);
            e
        })?;

        if let Some(child) = old_parent.children.lock().remove(&old_name) {
            *child.name.lock() = new_name.clone();
            *child.parent.lock() = Arc::downgrade(&new_parent);
            let new_ino = new_parent.inode.lock()
                .as_ref().map(|i| i.ino).unwrap_or(0);
            dcache().insert(new_ino, new_name.clone(), Arc::downgrade(&child));
            new_parent.children.lock().insert(new_name.clone(), child);
        }
        dcache().evict(old_ino, &old_name);
    }

    Ok(())
}

pub fn stat(path: &str) -> Result<Stat, VfsError> {
    let (_, dentry) = resolve_path(path)?;
    let inode_lock = dentry.inode.lock();
    let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    let st = inode.ops.getattr()?;
    inode.update_attr_from_stat(&st);
    Ok(st)
}

pub fn fstat(fd: u32) -> Result<Stat, VfsError> {
    let file = FD_TABLE.get(fd)?;
    let st = file.inode.ops.getattr()?;
    file.inode.update_attr_from_stat(&st);
    Ok(st)
}

pub fn dup(old_fd: u32) -> Result<u32, VfsError> {
    FD_TABLE.dup(old_fd)
}

pub fn dup2(old_fd: u32, new_fd: u32) -> Result<(), VfsError> {
    FD_TABLE.dup2(old_fd, new_fd)
}

pub fn sync_all() -> Result<(), VfsError> {
    for (_letter, mount) in DRIVE_MAP.iter() {
        mount.sb.ops.sync_fs()?;
    }
    Ok(())
}

fn sync_drive(drive: char) -> Result<(), VfsError> {
    let mount = DRIVE_MAP.lookup(drive)?;
    mount.sb.ops.sync_fs()
}

pub fn statfs(path: &str) -> Result<StatFs, VfsError> {
    let (letter, _) = resolve_path(path)?;
    let mount = DRIVE_MAP.lookup(letter)?;
    mount.sb.ops.statfs()
}

// ---------------------------------------------------------------------------
// Truncate
// ---------------------------------------------------------------------------

pub fn truncate(path: &str, len: u64) -> Result<(), VfsError> {
    let (_, dentry) = resolve_path(path)?;
    let inode_lock = dentry.inode.lock();
    let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
    inode.ops.truncate(len)?;
    inode.size.store(len, Ordering::Relaxed);
    Ok(())
}

pub fn ftruncate(fd: u32, len: u64) -> Result<(), VfsError> {
    let file = FD_TABLE.get(fd)?;
    file.inode.ops.truncate(len)?;
    file.inode.size.store(len, Ordering::Relaxed);
    Ok(())
}
