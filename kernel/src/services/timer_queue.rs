use alloc::collections::BinaryHeap;
use core::cmp::Ordering;

use super::universal_timer::{TimerCallback, TimerId};

/// A pending timer entry in the min-heap.
///
/// `TimerEntry` is `Send + Sync` because:
/// - `callback` is a function pointer (already `Sync`)
/// - `context` is an opaque pointer only dereferenced inside the
///   callback, which always runs on the BSP with the lock held
pub struct TimerEntry {
    pub id: TimerId,
    pub deadline: u64,
    pub period: Option<u64>,
    pub callback: TimerCallback,
    pub context: *mut u8,
}

unsafe impl Send for TimerEntry {}
unsafe impl Sync for TimerEntry {}

impl TimerEntry {
    pub fn new(
        id: TimerId,
        deadline: u64,
        period: Option<u64>,
        callback: TimerCallback,
        context: *mut u8,
    ) -> Self {
        TimerEntry { id, deadline, period, callback, context }
    }
}

impl Eq for TimerEntry {}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    /// Reverse ordering so `BinaryHeap` behaves as a min-heap on deadline.
    fn cmp(&self, other: &Self) -> Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

/// A min-heap of timer entries.  `next_deadline()` is O(1); insert and
/// cancel are O(log n).  Not thread-safe — the caller must provide
/// external synchronisation (e.g. `IrqMutex`).
pub struct TimerQueue {
    heap: BinaryHeap<TimerEntry>,
}

impl TimerQueue {
    pub fn new() -> Self {
        TimerQueue { heap: BinaryHeap::new() }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub fn insert(&mut self, entry: TimerEntry) {
        self.heap.push(entry);
    }

    /// Remove the entry with the given `id`.  Returns `true` if found and
    /// removed, `false` if the timer already fired or doesn't exist.
    ///
    /// This is O(n) — acceptable while the timer count stays modest.
    pub fn cancel(&mut self, id: TimerId) -> bool {
        let len = self.heap.len();
        self.heap.retain(|e| e.id != id);
        self.heap.len() != len
    }

    /// Peek at the earliest deadline without removing it.
    pub fn next_deadline(&self) -> Option<u64> {
        self.heap.peek().map(|e| e.deadline)
    }

    /// Remove and return all entries whose deadline has passed.
    pub fn drain_expired(&mut self, now_ns: u64) -> alloc::vec::Vec<TimerEntry> {
        let mut expired = alloc::vec::Vec::new();
        while let Some(peek) = self.heap.peek() {
            if peek.deadline <= now_ns {
                let entry = self.heap.pop().expect("heap empty after peek");
                expired.push(entry);
            } else {
                break;
            }
        }
        expired
    }
}
