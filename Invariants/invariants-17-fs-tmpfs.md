# tmpfs — Invariants

**Version:** 0.3.0
**Source:** `kernel/src/filesystems/fstypes/{mod,tmpfs}.rs`
**Status:** Stable

---

## State Invariants

**TMPFS-001 — Root inode number is 1:**
`ROOT_INO = 1`. All subsequent inodes are allocated from `NEXT_INO`
(starting at 2), incremented atomically.
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:16-17`

**TMPFS-002 — Inode numbers are unique (via atomic counter):**
`NEXT_INO` is an `AtomicU64`. `fetch_add(1, Relaxed)` provides
lock-free unique allocation.
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:16,124`

**TMPFS-003 — Per-inode locking for data and children:**
- `TmpfsEntry::File { data: Mutex<Vec<u8>> }` — file data protected
  by spinlock.
- `TmpfsEntry::Dir { children: Mutex<HashMap<String, Arc<TmpfsInode>>> }` —
  directory children protected by spinlock.
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:61-64`

**TMPFS-004 — File size is atomic: `size: AtomicU64`:**
Updated via `store(Relaxed)` on write, read via `load(Relaxed)`.
May be stale if read concurrent with a write on another CPU (but
writes are serialized by the data mutex).
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:72`

**TMPFS-005 — `create()` checks for duplicates:`
Returns `AlreadyExists` error if a child with the given name already
exists in the directory.
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:121-143`

**TMPFS-006 — `read_at` / `write_at` bounds-check:`
`read_at` clamps the read range to available data. `write_at` resizes
the backing `Vec` to fit the requested write.
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:75-106`

**TMPFS-007 — `lookup` errors on non-directory inodes:`
Returns `NotADirectory` if the inode is a file rather than a directory.
- Location: `kernel/src/filesystems/fstypes/tmpfs.rs:108-119`

---

## API Contracts

**TMPFS-API-001 — `Tmpfs::mount()`:**
Creates root directory inode (ino=1) with empty children HashMap.
Wraps in `Inode` + `SuperBlock`. Returns `(SuperBlock, InodeOps)`.

**TMPFS-API-002 — `FileSystem` trait:**
```rust
pub trait FileSystem: Sync {
    fn mount(&self, device: Option<Arc<dyn BlockDevice>>) -> Result<(SuperBlock, Arc<dyn InodeOps>), VfsError>;
    fn name(&self) -> &str;
}
```
- Location: `kernel/src/filesystems/fstypes/mod.rs`

---

## Design Notes

- tmpfs is a pure memory-backed filesystem. No block device needed.
- `mtime` is tracked per-inode via a `Mutex<u64>` (coarse but simple).
- Files and directories are both `TmpfsInode` with different `TmpfsEntry`
  variants, discriminated by `file_type`.
- No hard link support (each dentry owns its inode reference).
- `TmpfsInode` is stored behind `Arc` and referenced from `Inode::ops`.
