use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use spin::Once;

use super::capability::Capability;
use super::clockevent::Clockevent;
use super::clocksource::Clocksource;
use super::timer_queue::{TimerEntry, TimerQueue};

// ── Interrupt-safe lock wrapper ───────────────────────────────────
// Disables local IRQs while the inner Mutex is held, so the timer ISR
// (which also acquires this lock) cannot re-enter.

struct IrqSafeLock<T> {
    inner: Mutex<T>,
}

impl<T> IrqSafeLock<T> {
    const fn new(val: T) -> Self {
        IrqSafeLock { inner: Mutex::new(val) }
    }

    fn lock(&self) -> IrqSafeGuard<'_, T> {
        let was_enabled = crate::arch::CurrentArch::are_interrupts_enabled();
        if was_enabled {
            crate::arch::CurrentArch::disable_interrupts();
        }
        IrqSafeGuard { guard: Some(self.inner.lock()), was_enabled }
    }
}

struct IrqSafeGuard<'a, T> {
    guard: Option<spin::MutexGuard<'a, T>>,
    was_enabled: bool,
}

impl<'a, T> IrqSafeGuard<'a, T> {
    fn take_guard(&mut self) -> spin::MutexGuard<'a, T> {
        self.guard.take().expect("IrqSafeGuard already consumed")
    }
}

impl<T> Deref for IrqSafeGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().unwrap().deref()
    }
}

impl<T> DerefMut for IrqSafeGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().unwrap().deref_mut()
    }
}

impl<T> Drop for IrqSafeGuard<'_, T> {
    fn drop(&mut self) {
        let g = self.take_guard();
        drop(g);
        if self.was_enabled {
            crate::arch::CurrentArch::enable_interrupts();
        }
    }
}

// ── Exported types ────────────────────────────────────────────────

pub type TimerCallback = fn(context: *mut u8);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimerId(u64);

// ── UniversalTimer trait ──────────────────────────────────────────

pub trait UniversalTimer: Capability {
    /// Fire `callback(context)` at or after `deadline_ns` (absolute).
    fn set(&self, deadline_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId;

    /// Fire `callback(context)` every `interval_ns`, starting now.
    fn set_periodic(&self, interval_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId;

    /// Cancel a pending timer.  Returns `false` if the timer already fired
    /// or was never registered.
    fn cancel(&self, id: TimerId) -> bool;

    /// Current monotonic time in nanoseconds.
    fn now_ns(&self) -> u64;
}

// ── Implementation ────────────────────────────────────────────────

pub struct UniversalTimerImpl {
    clocksource: &'static dyn Clocksource,
    clockevent: &'static dyn Clockevent,
    queue: IrqSafeLock<TimerQueue>,
    next_id: AtomicU64,
}

impl UniversalTimerImpl {
    pub fn new(
        clocksource: &'static dyn Clocksource,
        clockevent: &'static dyn Clockevent,
    ) -> Self {
        UniversalTimerImpl {
            clocksource,
            clockevent,
            queue: IrqSafeLock::new(TimerQueue::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Process expired timers and reprogram the clockevent.
    ///
    /// Called from the timer ISR (interrupts disabled).
    pub fn tick(&self) {
        let now = self.clocksource.now_ns();
        let mut queue = self.queue.lock();

        let expired = queue.drain_expired(now);
        for entry in expired {
            (entry.callback)(entry.context);
            if let Some(period) = entry.period {
                queue.insert(TimerEntry::new(
                    entry.id,
                    now + period,
                    Some(period),
                    entry.callback,
                    entry.context,
                ));
            }
        }

        self.reprogram(&mut queue);
    }

    /// Program the clockevent to the earliest pending deadline.
    fn reprogram(&self, queue: &mut TimerQueue) {
        if let Some(next) = queue.next_deadline() {
            self.clockevent.set_deadline(next);
        } else {
            self.clockevent.stop();
        }
    }
}

impl Capability for UniversalTimerImpl {
    fn name(&self) -> &str {
        "universal-timer"
    }
}

impl UniversalTimer for UniversalTimerImpl {
    fn set(&self, deadline_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId {
        let id = TimerId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut queue = self.queue.lock();
        let old_next = queue.next_deadline();
        queue.insert(TimerEntry::new(id, deadline_ns, None, callback, context));
        if old_next != queue.next_deadline() {
            self.reprogram(&mut queue);
        }
        id
    }

    fn set_periodic(&self, interval_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId {
        let now = self.clocksource.now_ns();
        let deadline_ns = now + interval_ns;
        let id = TimerId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut queue = self.queue.lock();
        let old_next = queue.next_deadline();
        queue.insert(TimerEntry::new(id, deadline_ns, Some(interval_ns), callback, context));
        if old_next != queue.next_deadline() {
            self.reprogram(&mut queue);
        }
        id
    }

    fn cancel(&self, id: TimerId) -> bool {
        let mut queue = self.queue.lock();
        let old_next = queue.next_deadline();
        let removed = queue.cancel(id);
        if removed && old_next != queue.next_deadline() {
            self.reprogram(&mut queue);
        }
        removed
    }

    fn now_ns(&self) -> u64 {
        self.clocksource.now_ns()
    }
}

// ── Global singleton ──────────────────────────────────────────────

static UNIVERSAL_TIMER: Once<&'static UniversalTimerImpl> = Once::new();

/// Initialise the universal timer as early as possible.
///
/// Must be called exactly once, after the clocksource and clockevent are
/// ready but before interrupts are enabled on the BSP.
pub fn early_init(clocksource: &'static dyn Clocksource, clockevent: &'static dyn Clockevent) {
    let ut = alloc::boxed::Box::new(UniversalTimerImpl::new(clocksource, clockevent));
    let ut_static: &'static UniversalTimerImpl = alloc::boxed::Box::leak(ut);
    UNIVERSAL_TIMER.call_once(|| ut_static);

    // Wire the tick handler into the IDT timer ISR.  On x86_64 this is
    // the APIC vector 32 handler; on other platforms the analogous path.
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::idt::set_timer_callback(universal_timer_tick);
}

/// C function called from the timer ISR.  Dispatches to the singleton.
fn universal_timer_tick() {
    let ut = universal_timer_impl();
    ut.tick();
}

/// Return the global universal timer singleton.
///
/// Panics if `early_init()` has not been called.
pub fn universal_timer() -> &'static dyn UniversalTimer {
    *UNIVERSAL_TIMER.get().expect("UniversalTimer not initialised — call early_init() first")
}

/// Convenience: return the raw impl pointer for the ISR tick handler.
pub fn universal_timer_impl() -> &'static UniversalTimerImpl {
    *UNIVERSAL_TIMER.get().expect("UniversalTimer not initialised")
}
