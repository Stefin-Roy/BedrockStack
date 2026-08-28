use core::sync::atomic::{AtomicBool, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

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

use dentry::{Dentry, canonical_child_key, dcache};
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

/// Namespace lock: held across mount/unmount and across open()'s
/// resolve→attach sequence so an unmount cannot tear a drive down between
/// its final busy-check and DRIVE_MAP removal while an open() is attaching
/// to it.  PreemptMutex (preemption off, IRQs stay enabled) since it is held
/// across device I/O (sync/shutdown) where IRQ-disabled spinning would
/// stall interrupt delivery, but preemption must still be disabled on BSP.
pub static NS_LOCK: crate::sync::PreemptMutex<()> = crate::sync::PreemptMutex::new(());

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
        let (drive, start) = {
            let cwd = CWD.lock();
            let cwd = cwd.as_ref().ok_or(VfsError::NotFound)?;
            (cwd.drive, cwd.dentry.clone())
        };
        let components = path::split_components(path);
        let dentry = path::walk_from(start, &components)?;
        Ok((drive, dentry))
    }
}

/// Resolve parent dentry + leaf name from a path.
fn resolve_parent(path: &str) -> Result<(Arc<Dentry>, String), VfsError> {
    let (start_dentry, inner) = if let Ok((letter, inner)) = path::split_drive_path(path) {
        let mount = DRIVE_MAP.lookup(letter)?;
        (mount.root.clone(), inner)
    } else {
        let start = {
            let cwd = CWD.lock();
            cwd.as_ref().ok_or(VfsError::NotFound)?.dentry.clone()
        };
        (start, path)
    };

    let components = path::split_components(inner);
    if components.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    let leaf_name = String::from(*components.last().ok_or(VfsError::InvalidInput)?);
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

    // Set CWD to A> root
    let root = DRIVE_MAP.lookup('A')?.root.clone();
    *CWD.lock() = Some(CurrentWorkingDirectory {
        drive: 'A',
        dentry: root,
    });

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
    let _ns = NS_LOCK.lock();
    let fs = fstypes::lookup(fstype).ok_or(VfsError::NotFound)?;
    let (sb, root_ops) = fs.mount(device.clone())?;
    let root_inode = Arc::new(Inode::new(root_ops));
    let root_dentry = Dentry::new("", Some(root_inode));
    root_dentry.set_mount_point(true);

    let mid = path::next_mount_id();
    root_dentry.set_mount_id(mid);
    let mount = DriveMount::new(mid, root_dentry, sb.clone(), device);
    if let Err(e) = DRIVE_MAP.assign(drive, Arc::new(mount)) {
        // The superblock was fully constructed (FAT32 already set its
        // volume-dirty bit and scanned the FAT).  Tear it down so the
        // on-disk state does not look like an unclean shutdown.
        let _ = sb.ops.shutdown();
        return Err(e);
    }
    log::info!("VFS: mounted {} on {}>", fstype, drive);
    Ok(())
}

pub fn mount_virtual(source: &str, drive: char) -> Result<(), VfsError> {
    let _ns = NS_LOCK.lock();
    let (letter, src_dentry) = resolve_path(source)?;
    let src_inode = {
        let lock = src_dentry.inode.lock();
        lock.as_ref().cloned().ok_or(VfsError::NotFound)?
    };

    let src_mount = DRIVE_MAP.lookup(letter)?;
    let sb = src_mount.sb.clone();

    let bind_dentry = Dentry::new("", Some(src_inode));
    bind_dentry.set_mount_point(true);
    let mid = path::next_mount_id();
    bind_dentry.set_mount_id(mid);
    let mount = DriveMount::new(mid, bind_dentry, sb, None);
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
    let _ns = NS_LOCK.lock();
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
    target.set_mount_point(true);
    log::info!("VFS: mounted {} on {}> (at {})", fstype, drive, target_path);
    Ok(())
}

pub fn unmount(drive: char) -> Result<(), VfsError> {
    // Serialize against open()/mount()/unmount() for the whole teardown so
    // the final busy-check and DRIVE_MAP removal are atomic with respect to
    // new attaches.
    let _ns = NS_LOCK.lock();
    // Check CWD not on this drive
    {
        let cwd = CWD.lock();
        if let Some(ref cwd) = *cwd {
            if cwd.drive == drive {
                return Err(VfsError::MountBusy);
            }
        }
    }
    // Check no open FDs reference this drive BEFORE flushing/shutdown.
    // Do it before expensive FS operations to avoid wasted work; also
    // re-check after to close TOCTOU with concurrent open().
    let mount = DRIVE_MAP.lookup(drive)?;
    for fd in FD_TABLE.iter_active() {
        if dentry_belongs_to_mount(&fd.dentry, &mount.root) {
            return Err(VfsError::MountBusy);
        }
    }
    // Flush FS data (FAT cache, FSInfo, dirty bit) before unmount
    sync_drive(drive)?;
    {
        let mount = DRIVE_MAP.lookup(drive)?;
        mount.sb.ops.shutdown()?;
    }

    // Re-validate no new FD was opened between check and shutdown
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
    let target_mid = mount_root.get_mount_id();
    target_mid != 0 && dentry.get_mount_id() == target_mid
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
    *cwd = Some(CurrentWorkingDirectory {
        drive: letter,
        dentry,
    });
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
    // Hold the namespace lock across resolve→attach so a concurrent unmount
    // cannot complete between the busy-check and DRIVE_MAP removal while we
    // are pinning an inode on the dying mount.
    let _ns = NS_LOCK.lock();
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
        let p = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
        match p.ops.lookup(&leaf_name) {
            Ok(ops) => Some(ops),
            Err(VfsError::NotFound) => None,
            // With O_CREAT a transient lookup failure may just mean the file
            // does not exist yet; let create() surface the real error.  Any
            // other mode propagates device errors instead of masking them as
            // ENOENT.
            Err(_) if create => None,
            Err(e) => return Err(e),
        }
    };

    // Canonical cache key for the leaf under this parent (case-folded on
    // case-insensitive filesystems).
    let leaf_key = canonical_child_key(&parent, &leaf_name);

    // O_EXCL: fail if file already exists
    if flags.contains(OpenFlags::EXCL) && existing.is_some() {
        return Err(VfsError::AlreadyExists);
    }

    let mut inode: Arc<Inode> = match existing {
        Some(child_ops) => {
            let inode = Arc::new(Inode::new(child_ops));
            if trunc {
                inode.ops.truncate(0)?;
                inode.size.store(0, Ordering::Relaxed);
            }
            // Ensure the leaf is present in the parent's children map: opening
            // an existing file whose exact path was never dentry-resolved (it
            // may have been seen only via readdir, or the walk used different
            // letter-case than this path) must not fail on the fd dentry.
            let mut children = parent.children.lock();
            match children.get(&leaf_key).cloned() {
                Some(cd) => {
                    *cd.inode.lock() = Some(inode.clone());
                }
                None => {
                    let cd = Dentry::new(&leaf_name, Some(inode.clone()));
                    *cd.parent.lock() = Arc::downgrade(&parent);
                    cd.set_mount_id(parent.get_mount_id());
                    let parent_ino = parent.inode.lock().as_ref().map(|i| i.ino).unwrap_or(0);
                    dcache().insert(parent_ino, leaf_key.clone(), Arc::downgrade(&cd));
                    children.insert(leaf_key.clone(), cd);
                }
            }
            drop(children);
            inode
        }
        None => {
            if !create {
                return Err(VfsError::NotFound);
            }
            let child_ops = match {
                let lock = parent.inode.lock();
                let p = lock.as_ref().ok_or(VfsError::NotFound)?;
                p.ops.create(&leaf_name)
            } {
                Ok(ops) => ops,
                Err(VfsError::AlreadyExists) if !flags.contains(OpenFlags::EXCL) => {
                    // Raced with another create — use the existing file
                    let lock = parent.inode.lock();
                    let p = lock.as_ref().ok_or(VfsError::NotFound)?;
                    p.ops.lookup(&leaf_name)?
                }
                Err(e) => return Err(e),
            };
            let inode = Arc::new(Inode::new(child_ops));
            let child_dentry = Dentry::new(&leaf_name, Some(inode.clone()));
            *child_dentry.parent.lock() = Arc::downgrade(&parent);
            child_dentry.set_mount_id(parent.get_mount_id());
            parent
                .children
                .lock()
                .insert(leaf_key.clone(), child_dentry.clone());
            let parent_ino = parent.inode.lock().as_ref().map(|i| i.ino).unwrap_or(0);
            dcache().insert(parent_ino, leaf_key.clone(), Arc::downgrade(&child_dentry));
            inode
        }
    };

    let mut fd_dentry = parent
        .children
        .lock()
        .get(&leaf_key)
        .cloned()
        .ok_or(VfsError::NotFound)?;

    // Follow a final-component symlink (mid-path links were already followed
    // by walk_from).  O_NOFOLLOW refuses the first hop.
    if inode.file_type == FileType::Symlink {
        if flags.contains(OpenFlags::NOFOLLOW) {
            return Err(VfsError::Loop);
        }
        let mut loops = 0u32;
        while inode.file_type == FileType::Symlink {
            loops += 1;
            if loops > path::SYMLINK_MAX {
                return Err(VfsError::Loop);
            }
            let target = inode.ops.readlink()?;
            if target.is_empty() {
                return Err(VfsError::Loop);
            }
            let resolved = if let Ok((letter, inner)) = path::split_drive_path(&target) {
                let mount = DRIVE_MAP.lookup(letter)?;
                path::walk_from(mount.root.clone(), &path::split_components(inner))?
            } else if target.starts_with('/') {
                // Absolute without drive letter: from the link's filesystem root.
                let mount_id = parent.get_mount_id();
                let base = if mount_id != 0 {
                    if let Some((_, m)) = DRIVE_MAP.lookup_by_id(mount_id) {
                        m.root.clone()
                    } else {
                        parent.clone()
                    }
                } else {
                    parent.clone()
                };
                path::walk_from(
                    base,
                    &path::split_components(target.trim_start_matches('/')),
                )?
            } else {
                // Relative targets resolve against the link's directory.
                path::walk_from(parent.clone(), &path::split_components(target.as_str()))?
            };
            fd_dentry = resolved;
            let next = fd_dentry.inode.lock().clone().ok_or(VfsError::NotFound)?;
            inode = next;
        }
    }

    // Structural open-flag enforcement (shape rules, not access control).
    if flags.contains(OpenFlags::DIRECTORY) && inode.file_type != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    if flags.contains(OpenFlags::NOFOLLOW) && inode.file_type == FileType::Symlink {
        return Err(VfsError::Loop);
    }

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
    if file.inode.file_type == FileType::Directory {
        return Err(VfsError::IsADirectory);
    }
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
        // O_SYNC: durability before write() returns.
        if file.flags.contains(OpenFlags::SYNC) {
            file.inode.ops.flush()?;
        }
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
    let new_pos = match whence {
        SeekFrom::Start(o) => o as i64,
        SeekFrom::Current(o) => (*pos as i64).checked_add(o).ok_or(VfsError::InvalidInput)?,
        SeekFrom::End(o) => (file.inode.ops.size() as i64)
            .checked_add(o)
            .ok_or(VfsError::InvalidInput)?,
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
    child.set_mount_id(parent.get_mount_id());
    let dir_key = canonical_child_key(&parent, &name);
    parent.children.lock().insert(dir_key.clone(), child.clone());

    let parent_ino = parent.inode.lock().as_ref().map(|i| i.ino).unwrap_or(0);
    dcache().insert(parent_ino, dir_key, Arc::downgrade(&child));
    Ok(())
}

pub fn rmdir(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path)?;
    if name == "." || name == ".." {
        return Err(VfsError::InvalidInput);
    }
    let parent_ino = parent
        .inode
        .lock()
        .as_ref()
        .map(|i| i.ino)
        .ok_or(VfsError::NotFound)?;
    let child_key = canonical_child_key(&parent, &name);

    // Signal the child inode that it will be unlinked, before dropping the
    // dentry reference (which may drop the inode if no handles are open).
    if let Some(child) = parent.children.lock().get(&child_key) {
        if let Some(ref inode) = *child.inode.lock() {
            inode.ops.on_unlink();
        }
    }

    if let Some(child) = parent.children.lock().remove(&child_key) {
        child.inode.lock().take();
    }

    let parent_inode = parent.inode.lock();
    let p = parent_inode.as_ref().ok_or(VfsError::NotFound)?;
    p.ops.rmdir(&name)?;
    drop(parent_inode);

    dcache().evict(parent_ino, &child_key);
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
    let parent_ino = dentry
        .parent
        .lock()
        .upgrade()
        .and_then(|p| p.inode.lock().as_ref().map(|i| i.ino))
        .unwrap_or(inode.ino);

    entries.insert(
        0,
        DirEntry {
            ino: parent_ino,
            name: String::from(".."),
            file_type: FileType::Directory,
        },
    );
    entries.insert(
        0,
        DirEntry {
            ino: inode.ino,
            name: String::from("."),
            file_type: FileType::Directory,
        },
    );

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Namespace operations
// ---------------------------------------------------------------------------

pub fn unlink(path: &str) -> Result<(), VfsError> {
    let (parent, name) = resolve_parent(path)?;
    let parent_ino = parent
        .inode
        .lock()
        .as_ref()
        .map(|i| i.ino)
        .ok_or(VfsError::NotFound)?;
    let child_key = canonical_child_key(&parent, &name);

    // Reject unlinking directories (use rmdir instead)
    if let Some(child) = parent.children.lock().get(&child_key) {
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

    // Signal the child inode before dropping the dentry reference.
    if let Some(child) = parent.children.lock().get(&child_key) {
        if let Some(ref inode) = *child.inode.lock() {
            inode.ops.on_unlink();
        }
    }

    if let Some(child) = parent.children.lock().remove(&child_key) {
        child.inode.lock().take();
    }

    let parent_inode = parent.inode.lock();
    let p = parent_inode.as_ref().ok_or(VfsError::NotFound)?;
    p.ops.unlink(&name)?;
    drop(parent_inode);

    dcache().evict(parent_ino, &child_key);
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
        let old_key = canonical_child_key(old_parent.as_ref(), &old_name);
        let new_key = canonical_child_key(old_parent.as_ref(), &new_name);
        let mut children = old_parent.children.lock();
        if let Some(child) = children.remove(&old_key) {
            *child.name.lock() = new_name.clone();
            children.insert(new_key.clone(), child.clone());
            drop(children);
            dcache().evict(old_ino, &old_key);
            dcache().insert(old_ino, new_key, Arc::downgrade(&child));
        } else {
            drop(children);
            dcache().evict(old_ino, &old_key);
        }
    } else {
        // Different directories.  Try a native cross-directory rename; a
        // filesystem that cannot (or a different mount) returns EXDEV and we
        // propagate it -- VFS never byte-copies as a rename fallback, so a
        // crash can never leave a duplicate behind.
        let old_key_pre = canonical_child_key(old_parent.as_ref(), &old_name);

        // Cycle guard: refuse moving a directory into itself or its own
        // subtree.  The original check only looked at the dentry cache, so
        // an uncached directory (e.g. never readdir'd) could be moved into
        // its own child and create a cycle.  We now also consult the
        // filesystem via lookup() and compare inode identities.
        {
            let child_dentry_opt = old_parent.children.lock().get(&old_key_pre).cloned();
            let is_dir = if let Some(ref child) = child_dentry_opt {
                let g = child.inode.lock();
                g.as_ref().map(|i| i.file_type) == Some(FileType::Directory)
            } else {
                match old_ops.lookup(&old_name) {
                    Ok(ops) => ops.file_type() == FileType::Directory,
                    Err(_) => false,
                }
            };
            if is_dir {
                let child_ino_opt = if let Some(ref child) = child_dentry_opt {
                    child.inode.lock().as_ref().map(|i| i.ino)
                } else {
                    old_ops.lookup(&old_name).ok().map(|ops| ops.ino())
                };
                let mut anc = Some(new_parent.clone());
                while let Some(a) = anc {
                    if let Some(ref child) = child_dentry_opt {
                        if Arc::ptr_eq(&a, child) {
                            return Err(VfsError::InvalidInput);
                        }
                    }
                    if let Some(child_ino) = child_ino_opt {
                        let anc_ino = a.inode.lock().as_ref().map(|i| i.ino);
                        if anc_ino == Some(child_ino) {
                            return Err(VfsError::InvalidInput);
                        }
                    }
                    anc = a.parent.lock().upgrade();
                }
            }
        }

        old_ops.rename_across_dirs(new_ops.as_ref(), &old_name, &new_name)?;

        let old_key = canonical_child_key(old_parent.as_ref(), &old_name);
        let new_key = canonical_child_key(new_parent.as_ref(), &new_name);
        let new_ino = new_parent
            .inode
            .lock()
            .as_ref()
            .map(|i| i.ino)
            .unwrap_or(0);
        if let Some(child) = old_parent.children.lock().remove(&old_key) {
            *child.name.lock() = new_name.clone();
            *child.parent.lock() = Arc::downgrade(&new_parent);
            child.set_mount_id(new_parent.get_mount_id());
            dcache().insert(new_ino, new_key.clone(), Arc::downgrade(&child));
            new_parent.children.lock().insert(new_key, child);
        }
        dcache().evict(old_ino, &old_key);
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
    // Dedupe by superblock identity: bind-style mounts (mount_virtual) share
    // one SuperBlock across letters and must be flushed exactly once.
    let mut seen: Vec<usize> = Vec::new();
    for (_letter, mount) in DRIVE_MAP.iter() {
        let id = Arc::as_ptr(&mount.sb) as *const () as usize;
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
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
