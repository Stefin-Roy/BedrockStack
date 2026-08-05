use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Once;

use super::clockevent::Clockevent;
use super::clocksource::Clocksource;
use super::irqsafe::IrqLock;
use super::lockorder;
use super::timer_queue::{TimerEntry, TimerQueue};

// ── Exported types ────────────────────────────────────────────────

pub type TimerCallback = fn(context: *mut u8);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimerId(u64);

// ── UniversalTimer trait ──────────────────────────────────────────

pub trait UniversalTimer: Send + Sync {
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
    queue: IrqLock<TimerQueue>,
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
            queue: IrqLock::with_order(TimerQueue::new(), lockorder::TIMER_QUEUE),
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
                    now.saturating_add(period),
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
        let deadline_ns = now.saturating_add(interval_ns);
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

/// True once `early_init()` has run and the singleton is usable.
pub fn is_ready() -> bool {
    UNIVERSAL_TIMER.get().is_some()
}

/// Convenience: return the raw impl pointer for the ISR tick handler.
pub fn universal_timer_impl() -> &'static UniversalTimerImpl {
    *UNIVERSAL_TIMER.get().expect("UniversalTimer not initialised")
}

// ── Blocking waits ────────────────────────────────────────────────
//
// These register a one-shot timer that sets a static wake flag, then HLT
// until it fires.  The timer ISR wakes us; we never busy-spin on the TSC.

static WAKE_FLAG: AtomicBool = AtomicBool::new(false);

fn wake_callback(context: *mut u8) {
    unsafe {
        (*(context as *const AtomicBool)).store(true, Ordering::SeqCst);
    }
}

/// Wait until `done()` returns true, or `deadline_ns` (absolute) passes.
///
/// Yields the CPU via HLT, waking on device IRQs or a 1 ms periodic wake
/// timer, and re-evaluates `done()` after each wake.  This means pure
/// register-poll waits (port reset, controller start) progress even when no
/// interrupt is generated.  Returns `true` if `done()` became true before
/// the deadline.
///
/// Falls back to a spin loop if IRQs are disabled (the timer ISR could
/// never run, so HLT would sleep forever).
pub fn wait_until_cond(deadline_ns: u64, done: &dyn Fn() -> bool) -> bool {
    if !crate::arch::CurrentArch::are_interrupts_enabled() {
        loop {
            if done() {
                return true;
            }
            if universal_timer().now_ns() >= deadline_ns {
                return false;
            }
            core::hint::spin_loop();
        }
    }

    // Periodic 1 ms wake: guarantees done() is re-checked at a bounded
    // cadence even if the hardware never raises an interrupt.
    let id = universal_timer().set_periodic(
        1_000_000,
        wake_callback,
        &WAKE_FLAG as *const AtomicBool as *mut u8,
    );
    loop {
        if done() {
            universal_timer().cancel(id);
            return true;
        }
        if universal_timer().now_ns() >= deadline_ns {
            universal_timer().cancel(id);
            // One last chance — the condition may have just become true.
            return done();
        }
        WAKE_FLAG.store(false, Ordering::SeqCst);
        while !WAKE_FLAG.load(Ordering::SeqCst) {
            crate::arch::CurrentArch::halt();
        }
    }
}

/// Block until `now_ns() >= deadline_ns` (absolute), yielding the CPU.
pub fn wait_until(deadline_ns: u64) {
    wait_until_cond(deadline_ns, &|| false);
}

/// Block for `ms` milliseconds, yielding the CPU.
pub fn sleep_ms(ms: u64) {
    let now = universal_timer().now_ns();
    wait_until(now.saturating_add(ms.saturating_mul(1_000_000)));
}

/// Current monotonic time in nanoseconds.
pub fn now_ns() -> u64 {
    universal_timer().now_ns()
}
