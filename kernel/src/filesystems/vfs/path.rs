use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::dentry::Dentry;
use super::drive::DriveMap;
use super::error::VfsError;
use super::inode::Inode;
use super::irq::IrqMutex;
use super::mount::DriveMount;

static NEXT_MOUNT_ID: IrqMutex<u64> = IrqMutex::new(1);

pub fn next_mount_id() -> u64 {
    let mut id = NEXT_MOUNT_ID.lock();
    let val = *id;
    *id += 1;
    val
}

/// Parse "X>rest/of/path" into (drive_letter, inner_path).
pub fn split_drive_path(path: &str) -> Result<(char, &str), VfsError> {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes[1] != b'>' {
        return Err(VfsError::InvalidInput);
    }
    let letter = bytes[0] as char;
    if !letter.is_ascii_alphabetic() {
        return Err(VfsError::InvalidInput);
    }
    Ok((letter, &path[2..]))
}

/// Split a path string into non-empty components.
pub fn split_components(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Walk the dentry tree starting from `start`, resolving each path component.
/// Returns the final dentry.
pub fn walk_from(start: Arc<Dentry>, components: &[&str], dcache: &super::Dcache)
    -> Result<Arc<Dentry>, VfsError>
{
    let mut current = start;
    for &name in components {
        // 1. Check dcache
        let cur_ino = {
            let inode_lock = current.inode.lock();
            inode_lock.as_ref().map(|i| i.ino).unwrap_or(0)
        };
        if let Some(cached) = dcache.lookup(cur_ino, name) {
            if cached.is_negative() {
                return Err(VfsError::NotFound);
            }
            current = cached;
            continue;
        }

        // 2. Ask parent inode to look up
        let child_ops = {
            let inode_lock = current.inode.lock();
            let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
            inode.ops.lookup(name)?
        };

        // 3. Wrap in Inode, create Dentry, cache
        let child_inode = {
            let inode_lock = current.inode.lock();
            let parent_inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
            let sb = parent_inode.sb.clone();
            Arc::new(Inode::new(child_ops, sb))
        };
        let child = Dentry::new(name, Some(child_inode));
        {
            let mut children = current.children.lock();
            children.push(child.clone());
        }
        dcache.insert(cur_ino, String::from(name), Arc::downgrade(&child));

        current = child;
    }
    Ok(current)
}

/// Resolve a drive-letter path to its target dentry.
///
/// Format: `X>path/to/file` or `X>` (root of drive X).
pub fn resolve(path: &str, drives: &DriveMap, dcache: &super::Dcache)
    -> Result<Arc<Dentry>, VfsError>
{
    let (letter, inner) = split_drive_path(path)?;
    let mount = drives.lookup(letter)?;
    if inner.is_empty() {
        return Ok(mount.root.clone());
    }
    let components = split_components(inner);
    walk_from(mount.root.clone(), &components, dcache)
}

/// Resolve parent dentry + leaf name from a drive-letter path.
///
/// For `X>folder/file.txt` returns `(parent_dentry, "file.txt")`.
/// For `X>file.txt` returns `(root_dentry, "file.txt")`.
/// For `X>` returns error (no leaf).
pub fn resolve_parent(path: &str, drives: &DriveMap, dcache: &super::Dcache)
    -> Result<(Arc<Dentry>, String), VfsError>
{
    let (letter, inner) = split_drive_path(path)?;
    let mount = drives.lookup(letter)?;

    if inner.is_empty() {
        return Err(VfsError::InvalidInput);
    }

    let components = split_components(inner);
    if components.is_empty() {
        return Err(VfsError::InvalidInput);
    }

    let leaf_name = String::from(*components.last().unwrap());
    let parent_components = &components[..components.len() - 1];

    let parent = if parent_components.is_empty() {
        mount.root.clone()
    } else {
        walk_from(mount.root.clone(), parent_components, dcache)?
    };

    Ok((parent, leaf_name))
}
