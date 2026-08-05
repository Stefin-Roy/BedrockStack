//! Lock-order keys for the VFS hierarchy.
//!
//! Values are used as the `order` argument of `IrqLock::with_order` and are
//! asserted to be strictly increasing on a per-CPU lockdep stack
//! (`crate::smp::lockdep_push`) when the `lockdep` feature is enabled.
//! A nested acquire must never take a lock with an order ≤ the most recently
//! held lock on this CPU.

/// File-descriptor table locks.
pub const FD_TABLE: u8 = 1;
/// Directory-entry (dcache) locks.
pub const DENTRY: u8 = 2;
/// Inode metadata locks.
pub const INODE: u8 = 3;
/// Open-file (position/state) locks.
pub const FILE: u8 = 4;
/// VFS mount-registry locks.
pub const MOUNT_REGISTRY: u8 = 5;
/// FAT32 cache locks.
pub const FAT_CACHE: u8 = 6;
/// Generic block-cache locks.
pub const BLOCK_CACHE: u8 = 7;
/// Universal-timer queue lock.
pub const TIMER_QUEUE: u8 = 8;
