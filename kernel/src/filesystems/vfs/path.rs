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
    // 0 is reserved (means "no mount"), never hand it out. Wrap safely.
    let next = val.checked_add(1).unwrap_or(1);
    *id = if next == 0 { 1 } else { next };
    // Ensure we never return 0 even on overflow path
    if val == 0 { 1 } else { val }
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
/// Automatically crosses into mount points (dentries with non-zero `mount_id`).
pub fn walk_from(start: Arc<Dentry>, components: &[&str]) -> Result<Arc<Dentry>, VfsError> {
    let mut current = start;

    for &name in components {
        if name == "." || name.is_empty() {
            continue;
        }

        if name == ".." {
            // If current is a mount root (its parent weak is empty but it has
            // a covered weak), ascend to the covered dentry's mount point.
            // This lets ".." escape a mount (e.g. A>/mnt/.. -> A>/).
            let parent_opt = {
                let guard = current.parent.lock();
                guard.upgrade()
            };
            if let Some(p) = parent_opt {
                current = p;
            } else {
                // No parent: maybe a mount root. Try covered.
                // Look up by mount_id of current's parent mount point?
                // We stored covered weakly in DriveMount.covered, but need to
                // find which mount's root == current. Use lookup_by_id on
                // current's mount_id is not correct; instead check if current
                // is a mounted root by searching DRIVE_MAP for matching root ptr.
                // Simpler: if get_mount_point and parent empty, try to find
                // mount where root ptr eq current and use its covered.
                let mut covered_parent: Option<Arc<Dentry>> = None;
                for (_, m) in super::DRIVE_MAP.iter() {
                    if Arc::ptr_eq(&m.root, &current) {
                        if let Some(w) = m.covered.lock().as_ref().and_then(|w| w.upgrade()) {
                            covered_parent = Some(w);
                        }
                        break;
                    }
                }
                if let Some(p) = covered_parent {
                    current = p;
                }
            }
            continue;
        }

        // 1. Check parent's children list first
        let found = {
            let children = current.children.lock();
            children.get(name).cloned()
        };
        if let Some(child) = found {
            current = attempt_mount_cross(child);
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
            current = attempt_mount_cross(cached);
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

        current = attempt_mount_cross(child);
    }

    Ok(current)
}

/// If `dentry` is a mount point, return the root dentry of the mounted
/// drive. Otherwise return `dentry` unchanged.
fn attempt_mount_cross(dentry: Arc<Dentry>) -> Arc<Dentry> {
    let mid = dentry.get_mount_id();
    if mid == 0 {
        return dentry;
    }
    if let Some((_, mount)) = super::DRIVE_MAP.lookup_by_id(mid) {
        mount.root.clone()
    } else {
        dentry
    }
}
