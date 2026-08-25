use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::dentry::{Dentry, canonical_child_key, dcache};
use super::error::VfsError;
use super::inode::Inode;
use super::irq::IrqMutex;
use super::types::FileType;

/// Maximum symlink expansions during one resolution (POSIX ELOOP bound).
pub const SYMLINK_MAX: u32 = 40;

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

/// Canonical cache key for `name` as a child of `dir`.  Uses the directory's
/// filesystem semantics (case folding for FAT32/NTFS) so the dentry tree and
/// dcache cannot hold two identities for one on-disk entry.
fn child_cache_key(dir: &Dentry, name: &str) -> String {
    canonical_child_key(dir, name)
}

/// Walk the dentry tree from `start`, resolving each component.
/// Supports `.` (skip) and `..` (go to parent, clamped at root).
/// Automatically crosses into mount points (dentries with non-zero `mount_id`).
/// Follows symlinks with a bounded expansion count (ELOOP after 40).
pub fn walk_from(start: Arc<Dentry>, components: &[&str]) -> Result<Arc<Dentry>, VfsError> {
    walk_inner(start, components, 0)
}

fn walk_inner(
    start: Arc<Dentry>,
    components: &[&str],
    mut loops: u32,
) -> Result<Arc<Dentry>, VfsError> {
    let mut current = start;
    let mut idx = 0usize;

    while idx < components.len() {
        let name = components[idx];
        idx += 1;

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

        let key = child_cache_key(&current, name);

        // 1. Check parent's children list first
        let found = {
            let children = current.children.lock();
            children.get(&key).cloned()
        };
        let child = match found {
            Some(c) => c,
            None => {
                // 2. Check global dcache
                let cur_ino = {
                    let inode_lock = current.inode.lock();
                    inode_lock.as_ref().map(|i| i.ino).unwrap_or(0)
                };
                let cached = dcache().lookup(cur_ino, &key);
                match cached {
                    Some(cached) => {
                        if cached.is_negative() {
                            return Err(VfsError::NotFound);
                        }
                        // Ensure it's also in the children map
                        {
                            let mut children = current.children.lock();
                            if !children.contains_key(&key) {
                                children.insert(key.clone(), cached.clone());
                            }
                        }
                        cached
                    }
                    None => {
                        // 3. Ask FS driver
                        let child_ops;
                        {
                            let inode_lock = current.inode.lock();
                            let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
                            child_ops = inode.ops.lookup(name)?;
                        }

                        let child_inode = Arc::new(Inode::new(child_ops));
                        // Display name is what the caller asked for; the cache
                        // key is canonical.
                        let child = Dentry::new(name, Some(child_inode));
                        *child.parent.lock() = Arc::downgrade(&current);
                        // Children inherit their parent's mount identity so
                        // unmount's busy-scan sees open files below the root.
                        child.set_mount_id(current.get_mount_id());
                        {
                            let mut children = current.children.lock();
                            children.insert(key.clone(), child.clone());
                        }
                        dcache().insert(cur_ino, key.clone(), Arc::downgrade(&child));
                        child
                    }
                }
            }
        };

        current = attempt_mount_cross(child);

        // Follow a symlink at this position: splice its target into the
        // remaining component stream and continue from the appropriate base.
        loop {
            let ft = {
                let inode_lock = current.inode.lock();
                match inode_lock.as_ref() {
                    Some(i) => i.file_type,
                    None => break,
                }
            };
            if ft != FileType::Symlink {
                break;
            }
            loops += 1;
            if loops > SYMLINK_MAX {
                return Err(VfsError::Loop);
            }
            let target = {
                let inode_lock = current.inode.lock();
                let inode = inode_lock.as_ref().ok_or(VfsError::NotFound)?;
                inode.ops.readlink()?
            };
            if target.is_empty() {
                return Err(VfsError::Loop);
            }
            if let Ok((letter, inner)) = split_drive_path(&target) {
                let mount = super::DRIVE_MAP.lookup(letter)?;
                let base = mount.root.clone();
                let mut rest = split_components(inner);
                rest.extend_from_slice(&components[idx..]);
                return walk_inner(base, &rest, loops);
            } else if target.starts_with('/') {
                // Absolute path without drive letter: resolve from the
                // symlink's filesystem root (same mount), not its parent.
                let mut rest = split_components(target.trim_start_matches('/'));
                rest.extend_from_slice(&components[idx..]);
                let mount_id = current.get_mount_id();
                let base = if mount_id != 0 {
                    if let Some((_, m)) = super::DRIVE_MAP.lookup_by_id(mount_id) {
                        m.root.clone()
                    } else {
                        current.parent.lock().upgrade().ok_or(VfsError::NotFound)?
                    }
                } else {
                    current.parent.lock().upgrade().ok_or(VfsError::NotFound)?
                };
                return walk_inner(base, &rest, loops);
            } else {
                let mut rest = split_components(&target);
                rest.extend_from_slice(&components[idx..]);
                // Relative targets resolve against the link's directory.
                let base = current.parent.lock().upgrade().ok_or(VfsError::NotFound)?;
                return walk_inner(base, &rest, loops);
            }
        }
    }

    Ok(current)
}

/// If `dentry` is a mount point, return the root dentry of the mounted
/// drive. Otherwise return `dentry` unchanged.
fn attempt_mount_cross(dentry: Arc<Dentry>) -> Arc<Dentry> {
    if !dentry.is_mount_point() {
        return dentry;
    }
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
