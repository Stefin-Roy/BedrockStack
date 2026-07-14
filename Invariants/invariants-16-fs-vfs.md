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
weak parent reference, mount-point flag, and optional `mount_id`
pointing to a `DriveMount` covering this dentry.
- Location: `kernel/src/filesystems/vfs/dentry.rs`

**VFS-013 — Mount point crossing in path resolution:**
`walk_from()` calls `attempt_mount_cross()` after resolving each path
component. If the dentry has a non-zero `mount_id`, it looks up the
corresponding `DriveMount` via `DriveMap::lookup_by_id()` and switches
to that mount's root dentry, enabling transparent traversal into
subdirectory-mounted filesystems.
- Location: `kernel/src/filesystems/vfs/path.rs:41-130`

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
New FDs are allocated at the lowest available index. `dup()` and
`dup2()` create a new `Arc` reference sharing the same
`FileDescription`.
- Location: `kernel/src/filesystems/vfs/fdtable.rs`

### Unmount

**VFS-012 — `unmount()` checks for active references:**
Before removing a drive, `unmount()` verifies that (a) CWD is not
on that drive and (b) no open FDs reference the drive's dentry
tree. Returns `MountBusy` if either check fails.
- Location: `kernel/src/filesystems/vfs/mod.rs:158-185`

### File Operations

**VFS-007 — `read()` / `write()` use `IrqMutex` for position:**
File position (`FileDescription.pos`) is protected by `IrqMutex`.
`read()` checks the `READ` flag; `write()` checks the `WRITE` flag.
- Location: `kernel/src/filesystems/vfs/mod.rs:279-317`

**VFS-011 — APPEND writes are serialized with per-inode lock:**
`write()` with `APPEND` flag reads `ops.size()` (the authoritative FS
size, not the VFS-cached size) and is serialized by
`Inode::append_lock` to prevent TOCTOU races between two concurrent
APPEND writers.
- Location: `kernel/src/filesystems/vfs/inode.rs:38`,
  `kernel/src/filesystems/vfs/mod.rs:301-314`

**VFS-008 — Inode size is an `AtomicU64`:**
Updated atomically during writes. Read without locks. This is safe
because writes are serialized by the inode ops lock, but `size.load`
can see stale values between the write and store.
- Location: `kernel/src/filesystems/vfs/inode.rs`

### Path Resolution

**VFS-009 — Absolute path format: `X>rest/of/path`:**
Drive letter, `>`, then path components separated by `/`. Relative
paths are resolved against CWD. Empty paths return `InvalidInput`.
- Location: `kernel/src/filesystems/vfs/mod.rs:46-66`

**VFS-010 — VFS init is idempotent:**
`VFS_INIT` AtomicBool is set to `true` only after all mount and
directory-creation operations succeed, preventing a broken partial
init from being treated as ready.
- Location: `kernel/src/filesystems/vfs/mod.rs:30,99-116`

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
Registers all filesystems, mounts `tmpfs` on `A>`, creates `A>tmp`,
allocates placeholder FDs 0/1/2 pointing to empty files in `A>tmp`,
sets CWD to `A>`. Returns `Err(VfsError)` on failure.

**VFS-API-002 — `InodeOps` trait:**
Required operations: `read_at`, `write_at`, `lookup`, `create`, `mkdir`,
`unlink`, `rmdir`, `readdir`, `rename`, `truncate`, `getattr`, `file_type`, `size`.

**VFS-API-003 — `SuperOps` trait:**
Required operations: `statfs`, `sync_fs`.

**VFS-API-003a — Mount operations:**
`mount` mounts a filesystem as a new drive letter. `mount_at` mounts
a filesystem on a subdirectory of an existing drive, setting the
target dentry's `mount_id` to enable cross-drive path traversal.
`mount_virtual` creates a bind-mount sharing an existing inode tree.
`unmount` removes a drive, clearing any covered dentry's `mount_id`.

**VFS-API-004 — Open file operations:**
`open`, `close`, `read`, `write`, `seek`, `truncate`, `ftruncate`,
`fstat`, `dup`, `dup2`. All operate on file descriptor indices from
`FD_TABLE`. `open()` supports `O_EXCL` flag which returns
`AlreadyExists` if `CREATE|EXCL` and the file already exists.

**VFS-API-005 — Directory operations:**
`mkdir`, `rmdir`, `readdir`, `chdir`, `getcwd`, `unlink`, `rename`, `stat`.

**VFS-API-006 — Superblock operations:**
`sync_all` iterates all mounted drives and calls `sync_fs()` on each.
`statfs(path)` resolves the drive for a path and returns filesystem
statistics via `statfs()`.

---

## Design Notes

- VFS currently only supports `tmpfs`. Block-backed filesystems are for
  future implementation.
- `/dev` is not populated; no device special file type exists. Console
  I/O goes through `SerialPort` and `Framebuffer` directly, not through VFS.
- Standard FDs 0/1/2 are placeholder files in `A>tmp/`. They will be
  replaced by console device nodes when a devfs is implemented.
- `rename()` across different drives (cross-device) copies data through
  a userspace-style buffer. Directories cannot be renamed across devices
  (`CrossDeviceLink` error).
- The `.` and `..` entries in `readdir()` are synthesized, not stored
  in the filesystem.
