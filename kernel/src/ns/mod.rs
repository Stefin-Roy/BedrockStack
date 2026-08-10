//! The namespace layer: per-task path→`FileOps` bindings plus the two-syscall
//! ABI's resolution engine.
//!
//! The kernel-root namespace ([`init_kernel_root`]) endows the kernel's own
//! view at boot; every task inherits a deep-copy snapshot at spawn
//! (`Namespace::child_of`). [`bind_mount_roots`] adds the concrete mount
//! roots (`A`, `esp`) once the boot mounts exist.

pub mod mem_ops;
pub mod namespace;
pub mod resolve;
pub mod serial_file;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::file_ops::FileOps;
use crate::filesystems::vfs::mount::DriveMount;
use crate::filesystems::vfs::types::{DirEntry, FileKind, RightsMask, Stat};

pub use namespace::{kernel_root_namespace, Binding, Comp, Namespace, NsError};
pub use resolve::{resolve_current, resolve_in, Resolved, MAX_COMPONENTS, MAX_PATH_LEN, USER_LIMIT};

// ── The /proc branch ─────────────────────────────────────────────────────
//
// `/proc` exposes the current task's tree under both `self` and directly:
// `proc:self/ctl`, `proc:1`, `proc:status`, ... The synthetic `proc_self`
// tree resolves `0/1/2` and the op files per-caller; this thin directory adds
// the `self` alias so `proc:self/...` addresses the same root.

struct ProcDir;

impl FileOps for ProcDir {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        950000
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let inner = crate::filesystems::vfs::synthetic::proc_self::proc_self();
        let mut entries = inner.readdir()?;
        entries.insert(
            0,
            DirEntry {
                ino: 950001,
                name: alloc::string::String::from("self"),
                file_kind: FileKind::Directory,
                rights: RightsMask::RW,
            },
        );
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let inner = crate::filesystems::vfs::synthetic::proc_self::proc_self();
        if name == "self" {
            return Ok(inner);
        }
        inner.lookup(name)
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: 950000, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the `/proc` branch root.
#[cfg(target_arch = "x86_64")]
pub fn proc_dir() -> Arc<dyn FileOps> {
    Arc::new(ProcDir)
}

// ── Kernel-root endowment ────────────────────────────────────────────────

/// Bind the static synthetic trees into the kernel root namespace. Runs once
/// in `Kernel::init()` after the higher-half switch (the trees are stateless
/// walkers over live registries, so no services beyond that are needed). The
/// concrete mount roots are added later by [`bind_mount_roots`].
pub fn init_kernel_root() {
    use crate::filesystems::vfs::synthetic;
    let root = Arc::new(Namespace::new());

    let _ = root.bind(vec![Comp::lit("dev")], synthetic::dev::dev_root(), RightsMask::R);
    let _ = root.bind(vec![Comp::lit("mem")], synthetic::mem::mem_root(), RightsMask::RW);
    let _ = root.bind(vec![Comp::lit("irq")], synthetic::irq::irq_root(), RightsMask::RW);
    let _ = root.bind(vec![Comp::lit("mnt")], synthetic::mnt::mnt_root(), RightsMask::RW);
    let _ = root.bind(vec![Comp::lit("pci")], synthetic::pci::pci_root(), RightsMask::R);
    let _ = root.bind(vec![Comp::lit("console")], serial_file::console_file(), RightsMask::W);
    let _ = root.bind(vec![Comp::lit("res")], mem_ops::res_root(), RightsMask::W);

    // x86_64-only: the task trees (they walk the `proc` scheduler).
    #[cfg(target_arch = "x86_64")]
    {
        let _ = root.bind(vec![Comp::lit("tasks")], synthetic::tasks::tasks_root(), RightsMask::RW);
        let _ = root.bind(vec![Comp::lit("proc")], proc_dir(), RightsMask::RW);
        // The self-root `:`: per-caller stream + op resolution (":1" is this
        // task's stdout; ":status"/":ctl" are this task's control ops).
        let _ = root.bind(
            vec![Comp::lit(":")],
            synthetic::proc_self::proc_self(),
            RightsMask::RW,
        );
    }

    namespace::init_root(root);
}

/// Bind the concrete boot-mount roots (`A`, `esp`) into the kernel root
/// namespace. Runs in `Kernel::run()` after the tmpfs + ESP mounts exist and
/// before the scheduler spawns any task, so every task inherits them.
pub fn bind_mount_roots() {
    rebind_mount("A");
    rebind_mount("esp");
}

/// Re-bind one mount name in the kernel root namespace (used after a mount
/// op replaces a mount, so future tasks see the fresh root).
pub fn rebind_mount(name: &str) {
    let mount = match crate::filesystems::vfs::get_mount(name) {
        Some(m) => m,
        None => return,
    };
    let ops = match mount_root_ops(&mount) {
        Some(o) => o,
        None => return,
    };
    let _ = kernel_root_namespace().bind(vec![Comp::lit(name)], ops, RightsMask::RW);
}

/// The root inode's `FileOps` from a mount's root dentry.
fn mount_root_ops(mount: &DriveMount) -> Option<Arc<dyn FileOps>> {
    mount.root.inode.lock().as_ref().map(|inode| inode.ops.clone())
}
