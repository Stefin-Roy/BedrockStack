use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::dentry::{Dentry, dcache};
use super::error::VfsError;
use super::inode::Inode;
use super::irq::IrqMutex;

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

/// Split a path into normalized components (no empties, no ., no trailing).
pub fn split_components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// Walk the dentry tree from `start`, resolving each component.
/// Supports `.` (skip) and `..` (go to parent, clamped at root).
pub fn walk_from(start: Arc<Dentry>, components: &[&str]) -> Result<Arc<Dentry>, VfsError> {
    let mut current = start;

    for &name in components {
        if name == "." || name.is_empty() {
            continue;
        }

        if name == ".." {
            let upgrade = {
                let guard = current.parent.lock();
                guard.upgrade()
            };
            if let Some(p) = upgrade {
                current = p;
            }
            continue;
        }

        // 1. Check parent's children list first
        let found = {
            let children = current.children.lock();
            children.get(name).cloned()
        };
        if let Some(child) = found {
            current = child;
            continue;
        }

        // 2. Check global dcache
        let cur_ino = {
            let inode_lock = current.inode.lock();
            inode_lock.as_ref().map(|i| i.ino).unwrap_or(0)
        };
        let cached = dcache().lookup(cur_ino, name);
        if let Some(cached) = cached {
            if cached.is_negative() {
                return Err(VfsError::NotFound);
            }
            // Ensure it's also in the children map
            {
                let mut children = current.children.lock();
                if !children.contains_key(name) {
                    children.insert(String::from(name), cached.clone());
                }
            }
            current = cached;
            continue;
        }

        // 3. Ask FS driver
        let child_ops;
        {
            let inode_lock = current.inode.lock();
            let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
            child_ops = inode.ops.lookup(name)?;
        }

        let child_inode = Arc::new(Inode::new(child_ops));
        let child = Dentry::new(name, Some(child_inode));
        *child.parent.lock() = Arc::downgrade(&current);
        {
            let mut children = current.children.lock();
            children.insert(String::from(name), child.clone());
        }
        dcache().insert(cur_ino, String::from(name), Arc::downgrade(&child));

        current = child;
    }

    Ok(current)
}
