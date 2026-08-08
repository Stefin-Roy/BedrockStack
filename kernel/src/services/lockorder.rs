//! Lock-order keys for the VFS and object-store hierarchies.
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
/// Object-store records map.
pub const OBJECT_RECORDS: u8 = 9;
/// Object-store cascade-seal map.
pub const OBJECT_CASCADE: u8 = 10;
/// Object-store deny-set.
pub const OBJECT_DENY: u8 = 11;
/// Scheduler run queue (the per-scheduler 3-level priority pool of parked
/// runnable tasks). Above the timer queue: the wake callback re-queues a
/// sleeping task while the timer ISR still holds the timer queue.
pub const RUN_QUEUE: u8 = 12;
/// Scheduler task registry (every task ever spawned, for cap resolution and
/// forensics).
pub const ALL_TASKS: u8 = 13;
/// Scheduler current-task slot (the running task's strong ref).
pub const CURRENT_TASK: u8 = 14;
/// Scheduler join-wait list on a task (tasks parked waiting for its death).
pub const JOINERS: u8 = 15;
