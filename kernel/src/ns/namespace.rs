//! The namespace core: per-task binding lists that map path prefixes onto
//! `FileOps` roots with a rights mask.
//!
//! A task resolves every syscall path through its own `Namespace`, which is a
//! deep-copy snapshot of the kernel root namespace taken at spawn (see
//! [`Namespace::child_of`]) plus any per-task overlays. The kernel itself is
//! the principal; there is no "boot domain table" anymore — [`ROOT_NS`] is the
//! endowment the kernel gives every task, and each task inherits it by value,
//! never by parent-chain shadowing (so a later kernel-root mutation cannot
//! retroactively leak into a task that already spawned).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Once;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::file_ops::FileOps;
use crate::filesystems::vfs::types::RightsMask;
use crate::services::irqsafe::IrqLock;

/// One path component in a binding's prefix: a literal token or a one-level
/// wildcard that matches exactly one token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comp {
    /// A literal component (e.g. `"dev"`, `":"`, `"esp"`).
    Lit(Box<str>),
    /// Matches exactly one arbitrary token.
    Wild,
}

impl Comp {
    /// A literal binding component.
    pub fn lit(s: &str) -> Comp {
        Comp::Lit(Box::from(s))
    }

    /// Whether this component matches a path token. `Lit` requires equality;
    /// `Wild` matches any single token.
    pub fn matches(&self, token: &str) -> bool {
        match self {
            Comp::Lit(s) => s.as_ref() == token,
            Comp::Wild => true,
        }
    }
}

/// One binding: a prefix (sequence of components), the `FileOps` root the
/// prefix resolves to, and the rights mask granted on the whole branch.
#[derive(Clone)]
pub struct Binding {
    pub comps: Vec<Comp>,
    pub ops: Arc<dyn FileOps>,
    pub rights: RightsMask,
}

/// A task's name-to-resource map.
///
/// `bindings` uses an `IrqLock` (spin + local IRQ disable) so a future ISR
/// path that must resolve a path shares the same lock discipline as the
/// capability table it replaces. `resolve` snapshots the binding list under
/// the lock and walks it outside, so a concurrent rebind cannot dangle a walk
/// (the walked `Arc<dyn FileOps>` nodes keep their directories alive).
pub struct Namespace {
    pub bindings: IrqLock<Vec<Binding>>,
    /// Reserved for parent-chain shadowing (unused in v1 — namespaces are
    /// deep-copied at spawn, see module docs).
    pub parent: Option<Arc<Namespace>>,
}

impl Namespace {
    /// An empty namespace.
    pub fn new() -> Self {
        Namespace {
            bindings: IrqLock::new(Vec::new()),
            parent: None,
        }
    }

    /// A fresh namespace that deep-copies `parent`'s bindings (Arc clones of
    /// every root). This is the spawn-time inheritance: the child sees exactly
    /// what the parent held at that instant, and later parent/root mutations
    /// never leak into it.
    pub fn child_of(parent: &Arc<Namespace>) -> Arc<Namespace> {
        let bindings = parent.bindings.lock().clone();
        Arc::new(Namespace {
            bindings: IrqLock::new(bindings),
            parent: Some(Arc::clone(parent)),
        })
    }

    /// Bind a prefix to a `FileOps` root with a rights mask. Re-binding the
    /// same prefix replaces the previous binding (last wins, in insertion
    /// order) so kernel-root remounts can refresh a branch.
    pub fn bind(
        &self,
        comps: Vec<Comp>,
        ops: Arc<dyn FileOps>,
        rights: RightsMask,
    ) -> Result<(), NsError> {
        if comps.is_empty() {
            return Err(NsError::BadPath);
        }
        self.bindings.lock().push(Binding { comps, ops, rights });
        Ok(())
    }
}

/// Namespace resolution failure. Mapped to `u64::MAX` (-1) at the syscall
/// boundary (v1 has a single error value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsError {
    NotFound,
    Denied,
    BadPath,
    NotDirectory,
    TooDeep,
    OutOfMemory,
}

impl From<VfsError> for NsError {
    fn from(e: VfsError) -> NsError {
        match e {
            VfsError::NotFound => NsError::NotFound,
            VfsError::InvalidInput | VfsError::NameTooLong => NsError::BadPath,
            VfsError::NoSpace | VfsError::FileTooLarge => NsError::OutOfMemory,
            VfsError::NotADirectory | VfsError::IsADirectory => NsError::NotDirectory,
            _ => NsError::Denied,
        }
    }
}

/// The kernel-root namespace: the endowment every task inherits at spawn.
static ROOT_NS: Once<Arc<Namespace>> = Once::new();

/// Access the kernel-root namespace (the kernel itself is the principal).
pub fn kernel_root_namespace() -> Arc<Namespace> {
    ROOT_NS.get().expect("namespace: kernel root not initialized").clone()
}

/// Install the kernel-root namespace (called once by `init_kernel_root`).
pub fn init_root(root: Arc<Namespace>) {
    ROOT_NS.call_once(|| root);
}
