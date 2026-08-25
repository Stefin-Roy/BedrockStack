# Invariant 28 — Storage Stack Coherence & Lifecycle

These invariants were established with the storage-stack hardening pass. They
are contracts: code that violates them is a bug even if it "works".

## I-28.1 Block-cache coherence (blockdriver/block_cache.rs)

1. Every successful WRITE through `CachedDevice` must either refresh the
   cached copy (single-sector, 512-byte `Buf`/`ConstBuf`) or invalidate every
   overlapping cached line (multi-sector writes, and ALL `IoBuffer::Phys`
   writes). A stale cached line after a completed write is a data-corruption
   bug.
2. Multi-sector reads bypass the cache by design (zero-copy DMA); they never
   populate lines. Single-sector reads (`read_raw`) are the only cache-fill
   path for reads.
3. Consecutive buffered writes in one `submit()` call are forwarded as ONE
   inner-device batch so AHCI can map them onto parallel NCQ slots. Cache
   update/invalidation happens only after the batch reports success; on
   partial failure the whole run's overlap is invalidated conservatively.

## I-28.2 BlockDevice trait surface (blockdriver/traits.rs)

4. `sector_size()` defaults to 512; all `IoRequest.count` values are denominated
   in units of the implementing device's logical sector size.
5. `sync()` must make device-owned write state durable before returning.
   Default is a no-op for cache-less devices. Filesystem-level durability
   (fsync/O_SYNC) layers on top of this.

## I-28.3 VFS namespace & mount lifecycle (vfs/)

6. Child dentries inherit their parent's `mount_id` at creation time. The
   unmount busy-scan therefore sees open files anywhere below a mount root;
   an unmount with open FDs inside the tree MUST return `MountBusy`.
7. All of `open()`'s resolve→attach sequence, and the entire bodies of
   `mount`, `mount_at`, `mount_virtual`, `unmount`, are serialized by
   `vfs::NS_LOCK` (a plain `spin::Mutex`, NEVER an `IrqMutex` — it is held
   across blocking device I/O where IRQ-disabled spinning would stall
   interrupt delivery). No code path may acquire NS_LOCK while holding a
   filesystem-internal lock.
8. A failed `vfs::mount` (e.g., duplicate drive letter) MUST invoke
   `sb.ops.shutdown()`: the FAT32 superblock sets its volume-dirty bit during
   construction, so dropping without teardown leaves the volume falsely
   flagged "not cleanly unmounted".
9. Dentry-tree and dcache keys use `InodeOps::canonical_name(parent)`. Any
   filesystem whose lookup is case-insensitive MUST override `canonical_name`
   to fold case identically to its on-disk matching, and MUST key any inode-
   number allocation (FAT32 `ino_for`) on the folded name. Violations produce
   two identities for one dirent.
10. Cross-mount renames return `CrossDeviceLink`. VFS NEVER byte-copies as a
    rename fallback. Same-superblock cross-directory renames go through
    `InodeOps::rename_across_dirs`; implementations lock both directories in
    deterministic order and refuse different superblocks via `as_any`
    downcast identity.
11. `OpenFlags` is u32 with bits defined in `types.rs`; `KNOWN_MASK` bounds
    what the kernel interprets. Unispace rejects unknown bits before they
    reach VFS. `O_DIRECTORY` → ENOTDIR on non-directories; `O_NOFOLLOW` →
    ELOOP (`VfsError::Loop`) when the final component is a symlink; `O_SYNC`
    flushes via `InodeOps::flush` before `write()` returns.
12. Path resolution follows symlinks with at most `SYMLINK_MAX` (40)
    expansions per resolution; relative targets resolve against the link's
    parent directory, absolute targets against the named drive root.

## I-28.4 FAT32 handle lifecycle & crash safety (fstypes/fat32/)

13. Live inode handles are registered per `(parent_clus, case-folded name)`
    in `Fat32SuperBlock.handle_registry`. On unlink/rmdir:
    - live handles found → each marked `unlinked`; their Drop frees exactly
      one chain;
    - no live handles → the FS frees the chain immediately (orphaned-name
      leak prevention).
14. A handle whose `unlinked` flag is set MUST NOT write directory-entry
    metadata back (`update_dirent_meta` / `sync_clus_and_size` no-op). The
    dirent may since belong to a recreated file; name-based updates would
    stomp the new file's first_clus/size. Data writes continue into the
    handle's own (still-allocated) chain.
15. Rename-over-existing marks the overwritten file's handles unlinked
    BEFORE freeing its chain, so Drop cannot double-free.
16. Directory growth (`place_with_growth`) must convert the old last
    cluster's DIR_END slot into real entry data (or overwrite it), because
    readers stop at the first DIR_END and never follow the FAT chain past
    it. Entry groups ([LFN…]+SFN) are never split across clusters; a group
    larger than one cluster returns InvalidInput instead of panicking.
17. LFN chains whose checksum does not match their SFN are discarded on read
    (fall back to the SFN). Names needing >20 LFN slots fail with
    InvalidInput at create/rename time.
18. Cluster allocation uses the in-RAM `alloc_bitmap` (bit set = allocated),
    built during the mount-time free scan and capped at 16M clusters (~2 MiB).
    `free_chain` clears bits; `alloc_cluster` rolls back a reservation if the
    FAT write fails. Volumes above the cap transparently use the legacy
    FAT-scan allocator.
19. `FatCache::maybe_evict` must always terminate: a full clean sweep that
    evicts nothing flushes a batch of dirty sectors synchronously and retries
    once. An all-dirty cache must never spin while holding the FatCache mutex.
20. Per-inode `write_lock` is an RwLock: readers take `.read()` (concurrent),
    writers/truncates take `.write()`.

## I-28.5 Partition tables (partition/)

21. Protective-MBR detection scans all four primary slots for type 0xEE;
    MBR parsing skips 0xEE entries so a corrupt GPT falling back to MBR never
    surfaces bogus partitions.
22. Partition entries with `sector_count == 0` are skipped (never compute
    `start + count - 1`). All ranges are checked-add and contained within the
    device; EBR logicals must sit inside the extended partition extent.
23. Overlapping real partitions reject the whole table ("overlapping
    partitions").
24. GPT: header CRC validated; header `my_lba` must match the sector it was
    read from; `partition_entries_crc32` validated over the full entry array;
    primary failure falls back to the backup header at `device.sector_count()-1`.
25. Mount-attempt errors carry their detail inline (`MountAttemptError`);
    process-global "last error" slots are prohibited (they clobber across
    sequential attempts).

## I-28.6 Tmpfs (fstypes/tmpfs)

26. Growth beyond `TMPFS_BUDGET` fails with `NoSpace` in write_at/truncate.
    The budget is enforced accounting, not a statfs fiction.
27. Symlink targets are raw bytes (`Vec<u8>`). Only the String-typed readlink
    API may render them lossily.
