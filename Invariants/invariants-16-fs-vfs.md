# VFS Core — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/filesystems/vfs/{mod,dentry,inode,superblock,file,fdtable,mount,drive,path,irq,types,error}.rs`
**Status:** Stable

---

## State Invariants

### Locking

**VFS-001 — `IrqMutex` disables interrupts during critical sections:**
`IrqMutex::lock()` saves interrupt state, disables interrupts, acquires
the `spin::Mutex`, and restores interrupt state on drop. This prevents
deadlock between interrupt handlers and VFS code.
- Location: `kernel/src/filesystems/vfs/irq.rs:6-59`

**VFS-002 — Lock ordering (same-parent same-inode avoidance):**
The `rename()` function detects when `old_parent == new_parent` (same
`Arc`) and takes the inode lock once to avoid re-locking a non-reentrant
`spin::Mutex`.
- Location: `kernel/src/filesystems/vfs/mod.rs:443-451`

### Dentry and Dcache

**VFS-003 — Dentry children are stored in `HashMap<String, Arc<Dentry>>`:**
Protected by `IrqMutex`. Each child stores its name, optional inode,
weak parent reference, and mount-point flag.
- Location: `kernel/src/filesystems/vfs/dentry.rs`

**VFS-004 — Dcache is initialized exactly once via `spin::Once`:**
The dentry cache (`Dcache`) maps `(parent_ino, name) → Weak<Dentry>`.
Used for quick path component lookup.
- Location: `kernel/src/filesystems/vfs/dentry.rs`

### Drive Map

**VFS-005 — Drive letters `A:` through `Z:`:**
Each letter maps to an `Arc<DriveMount>` containing root dentry,
superblock, and optional block device. Protected by `IrqMutex`.
- Location: `kernel/src/filesystems/vfs/drive.rs`

### File Descriptors

**VFS-006 — FD table allocates monotonically increasing indices:**
`FdTable` wraps a `Vec<Option<FileDescription>>` behind `IrqMutex`.
New FDs are allocated at the lowest available index.
- Location: `kernel/src/filesystems/vfs/fdtable.rs`

### File Operations

**VFS-007 — `read()` / `write()` use `IrqMutex` for position:**
File position (`FileDescription.pos`) is protected by `IrqMutex`.
`write()` with `APPEND` flag always writes at the current end-of-file
(size) rather than the current position.
- Location: `kernel/src/filesystems/vfs/mod.rs:279-311`

**VFS-008 — Inode size is an `AtomicU64`:**
Updated atomically during writes. Read without locks. This is safe
because writes are serialized by the inode ops lock, but `size.load`
can see stale values between the write and store.
- Location: `kernel/src/filesystems/vfs/inode.rs`

### Path Resolution

**VFS-009 — Absolute path format: `X>rest/of/path`:**
Drive letter, `>`, then path components separated by `/`. Relative
paths are resolved against CWD.
- Location: `kernel/src/filesystems/vfs/mod.rs:46-66`

**VFS-010 — VFS init is idempotent:**
`VFS_INIT` AtomicBool prevents double initialization.
- Location: `kernel/src/filesystems/vfs/mod.rs:30,99-103`

---

## Safety Invariants

**VFS-S001 — `InodeOps::from_payload` safety (if used):**
The payload pointer must be valid and aligned, pointing to a
`TmpfsInode` (or other FS-specific inode data). The Tmpfs
implementation passes `Arc::as_ptr()` which is valid for the
`Arc`'s lifetime.
- Location: `kernel/src/filesystems/vfs/inode.rs`

---

## API Contracts

**VFS-API-001 — `vfs::init()`:**
Registers all filesystems, mounts `tmpfs` on `A>`, creates `A>tmp`
and `A>dev` directories, sets CWD to `A>`. Returns `Err(VfsError)`
on failure.

**VFS-API-002 — `InodeOps` trait:**
Required operations: `read_at`, `write_at`, `lookup`, `create`, `mkdir`,
`unlink`, `rmdir`, `readdir`, `rename`, `truncate`, `getattr`, `file_type`, `size`.

**VFS-API-003 — `SuperOps` trait:**
Required operations: `statfs`, `sync_fs`.

**VFS-API-004 — Open file operations:**
`open`, `close`, `read`, `write`, `seek`, `truncate`, `ftruncate`.
All operate on file descriptor indices from `FD_TABLE`.

**VFS-API-005 — Directory operations:**
`mkdir`, `rmdir`, `readdir`, `chdir`, `getcwd`, `unlink`, `rename`, `stat`.

---

## Design Notes

- VFS currently only supports `tmpfs`. Block-backed filesystems are for
  future implementation.
- `rename()` across different drives (cross-device) copies data through
  a userspace-style buffer. Directories cannot be renamed across devices
  (`CrossDeviceLink` error).
- The `.` and `..` entries in `readdir()` are synthesized, not stored
  in the filesystem.
