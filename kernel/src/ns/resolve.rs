//! Path resolution through a task's namespace.
//!
//! The wire path grammar (v1):
//! - Components are separated by `/` (tree descent) and `:` (op/self tokens).
//! - A leading `:` selects the self-root (`:1` = this task's stdout).
//! - Empty components are malformed (`tasks:100::status` is rejected).
//! - The final component is resolved by trying the literal name first and, on
//!   `NotFound`, the same name with a leading `:` — so `tasks/101/status`
//!   reaches the `:status` op file while a real on-disk `status` wins.
//!
//! Resolution is snapshot-then-walk: the binding list is cloned once under
//! its `IrqLock` and all per-node `FileOps::lookup` work happens outside it,
//! so a concurrent rebind can never dangle a walk (the resolved nodes keep
//! their directories alive via `Arc`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use alloc::format;

use crate::filesystems::vfs::file_ops::FileOps;
use crate::filesystems::vfs::types::RightsMask;

use super::namespace::{Binding, kernel_root_namespace, Comp, Namespace, NsError};

/// Maximum path length in bytes (excluding the NUL terminator).
pub const MAX_PATH_LEN: usize = 255;
/// Maximum number of path components.
pub const MAX_COMPONENTS: usize = 16;
/// Base of the low (user) canonical half — user pointers must stay below.
pub const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// The outcome of a successful resolution: the leaf node plus the rights mask
/// the caller holds on it (the matched binding's mask).
pub struct Resolved {
    pub ops: Arc<dyn FileOps>,
    pub rights: RightsMask,
}

/// Resolve `path` through the currently executing task's namespace (or the
/// kernel root when no task is running, e.g. in the boot path). Never
/// `.unwrap()`/`.expect()`es on user-supplied input.
pub fn resolve_current(path: &[u8]) -> Result<Resolved, NsError> {
    let ns = current_namespace();
    resolve_in(&ns, path)
}

/// The namespace to resolve through: the current task's if the scheduler is
/// live, otherwise the kernel root.
fn current_namespace() -> Arc<Namespace> {
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(task) = crate::proc::current_task() {
            return Arc::clone(&task.domain.ns);
        }
    }
    kernel_root_namespace()
}

/// Resolve a raw byte path against a specific namespace.
pub fn resolve_in(ns: &Arc<Namespace>, path: &[u8]) -> Result<Resolved, NsError> {
    let comps = tokenize(path)?;
    if comps.is_empty() {
        return Err(NsError::BadPath);
    }

    // Snapshot the bindings once, then walk outside the lock.
    let bindings = ns.bindings.lock().clone();

    // Longest-prefix match: a binding whose component list is a prefix of the
    // path wins over a shorter one; among equal-length prefixes the LAST in
    // insertion order wins (bind is "last wins", so a rebind_mount replaces
    // the earlier binding instead of being shadowed by it).
    let mut best: Option<(&Binding, usize)> = None;
    for b in &bindings {
        let blen = b.comps.len();
        if blen > comps.len() {
            continue;
        }
        let mut ok = true;
        for (i, bc) in b.comps.iter().enumerate() {
            if !bc.matches(&comps[i]) {
                ok = false;
                break;
            }
        }
        if ok && best.map_or(true, |(cur, _)| blen >= cur.comps.len()) {
            best = Some((b, blen));
        }
    }
    let (binding, blen) = best.ok_or(NsError::NotFound)?;

    // Base node: the binding's root, advancing through any wildcard comps.
    let mut node: Arc<dyn FileOps> = Arc::clone(&binding.ops);
    for (i, bc) in binding.comps.iter().enumerate() {
        if let Comp::Wild = bc {
            node = node.lookup(&comps[i]).map_err(|_| NsError::NotFound)?;
        }
    }

    // Walk the remaining components. The final one is leaf-disambiguated:
    // literal name first, then `:name` (the op files are `:status` etc.).
    // `leaf_via_op` records that the leaf is an op file, so its rights are
    // intersected with the op's advertised rights below.
    let mut leaf_via_op = false;
    for i in blen..comps.len() {
        let last = i == comps.len() - 1;
        if !last {
            node = node.lookup(&comps[i]).map_err(|_| NsError::NotFound)?;
        } else {
            let name = comps[i].as_str();
            node = match node.lookup(name) {
                Ok(n) => {
                    if name.starts_with(':') {
                        leaf_via_op = true;
                    }
                    n
                }
                Err(crate::filesystems::vfs::error::VfsError::NotFound) => {
                    let op = format!(":{}", name);
                    leaf_via_op = true;
                    node.lookup(&op).map_err(|_| NsError::NotFound)?
                }
                Err(_) => return Err(NsError::NotFound),
            };
        }
    }

    // An op leaf only grants what the op itself advertises: the caller's
    // binding rights are intersected with the op's `OpDesc` rights. Ops with
    // no matching descriptor default to write-only. Non-op leaves (regular
    // files, directories, streams) keep the binding mask unchanged, and the
    // self-root `:` binding is a directory, never an op, so it is untouched.
    let mut rights = binding.rights;
    if leaf_via_op {
        let last = match comps.last() {
            Some(s) => s.as_str(),
            None => return Err(NsError::BadPath),
        };
        let op_name = format!(":{}", last);
        let op_rights = match node.ops().iter().find(|d| d.name == op_name) {
            Some(d) => d.rights,
            None => RightsMask::W,
        };
        rights = rights.intersection(op_rights);
    }

    Ok(Resolved { ops: node, rights })
}

/// Split a raw path into components per the v1 grammar (see module docs).
fn tokenize(path: &[u8]) -> Result<Vec<String>, NsError> {
    if path.len() > MAX_PATH_LEN {
        return Err(NsError::BadPath);
    }
    let s = core::str::from_utf8(path).map_err(|_| NsError::BadPath)?;
    let mut comps: Vec<String> = Vec::new();
    let mut rest = s;
    // A leading `:` selects the self-root branch (keyed `":"`).
    if let Some(r) = s.strip_prefix(':') {
        comps.push(String::from(":"));
        rest = r;
    }
    for piece in rest.split(|c| c == '/' || c == ':') {
        if piece.is_empty() {
            return Err(NsError::BadPath);
        }
        comps.push(String::from(piece));
    }
    if comps.len() > MAX_COMPONENTS {
        return Err(NsError::TooDeep);
    }
    Ok(comps)
}
