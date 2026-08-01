//! Bounded lock-free MPSC event queue.
//!
//! Producers (drivers) call [`InputQueue::push`] from interrupt or poll
//! context; the single consumer calls [`InputQueue::pop`] from the main loop.
//! The queue is non-blocking: a full queue makes `push` return `false` (the
//! caller drops the event) instead of spinning.
//!
//! # Algorithm
//!
//! Vyukov-style bounded queue with per-slot round counters.  `head` and `tail`
//! are monotonic indices that grow without bound; a stream position `pos`
//! maps to slot `pos % CAPACITY` in round `pos / CAPACITY`.
//!
//! - Producer: CAS-claim `head` (only if `head - tail < CAPACITY`), wait until
//!   the slot's `seq == 2*round`, write the event, then publish with
//!   `seq.store(2*round + 1, Release)`.
//! - Consumer (single): read the slot only after `seq.load(Acquire) ==
//!   2*round + 1`, then release the slot for its next round with
//!   `seq.store(2*round + 2, Release)` before advancing `tail`.
//!
//! All slots start at `seq == 0` (round 0, "empty"), so the queue can be a
//! plain `static const` — no heap, no runtime builder, no large stack value.
//! Because there is exactly one consumer, `tail` needs no CAS; `head` is
//! claimed by CAS so at most one producer owns a given slot per round.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::event::InputEvent;

const CAPACITY: usize = 256;

struct Slot {
    /// Round counter for this slot: `2*round` = empty, `2*round + 1` = ready.
    seq: AtomicUsize,
    event: UnsafeCell<InputEvent>,
}

impl Slot {
    const fn new() -> Self {
        Slot {
            seq: AtomicUsize::new(0),
            event: UnsafeCell::new(InputEvent::zero()),
        }
    }
}

/// Lock-free bounded MPSC ring of [`InputEvent`]s.
pub struct InputQueue {
    slots: [Slot; CAPACITY],
    head: AtomicUsize,
    tail: AtomicUsize,
}

// Safety: the algorithm ensures each slot's `UnsafeCell` is written by at most
// one producer per round (CAS-claimed `head`) and read by the consumer only
// after observing the published `seq`.  The `seq` atomics create the
// happens-before edges between those accesses.  A producer running in an ISR
// can therefore coexist with the main-loop consumer without any lock.
unsafe impl Sync for InputQueue {}

impl InputQueue {
    pub const fn new() -> Self {
        InputQueue {
            slots: [const { Slot::new() }; CAPACITY],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Enqueue one event.  Returns `false` (and does not enqueue) if the queue
    /// is full.  Safe to call from interrupt context.
    pub fn push(&self, event: InputEvent) -> bool {
        loop {
            let head = self.head.load(Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Acquire);
            if head.wrapping_sub(tail) >= CAPACITY {
                return false; // full — caller drops the event
            }
            if self
                .head
                .compare_exchange_weak(head, head + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let slot = &self.slots[head % CAPACITY];
            let round = head / CAPACITY;
            // The slot must be empty for this round: the consumer released it
            // after finishing round `round - 1` by storing `2*round`.  Spin
            // only to satisfy memory ordering.
            while slot.seq.load(Ordering::Acquire) != 2 * round {
                core::hint::spin_loop();
            }
            unsafe { *slot.event.get() = event };
            slot.seq.store(2 * round + 1, Ordering::Release);
            return true;
        }
    }

    /// Dequeue one event, or `None` if the queue is empty.  Single-consumer.
    pub fn pop(&self) -> Option<InputEvent> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let slot = &self.slots[tail % CAPACITY];
        let round = tail / CAPACITY;
        // Wait until the producer has published the data for this round.
        while slot.seq.load(Ordering::Acquire) != 2 * round + 1 {
            core::hint::spin_loop();
        }
        let event = unsafe { *slot.event.get() };
        slot.seq.store(2 * round + 2, Ordering::Release);
        self.tail.store(tail + 1, Ordering::Release);
        Some(event)
    }

    /// Approximate number of buffered events.
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }
}
