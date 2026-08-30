use super::clockevent::Clockevent;
use super::clocksource::Clocksource;
use super::timer_queue::{TimerEntry, TimerQueue};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Mutex;
use spin::Once;

// ── Interrupt-safe lock wrapper ───────────────────────────────────
// Disables local IRQs while the inner Mutex is held, so the timer ISR
// (which also acquires this lock) cannot re-enter.

struct IrqSafeLock<T> {
    inner: Mutex<T>,
}

impl<T> IrqSafeLock<T> {
    const fn new(val: T) -> Self {
        IrqSafeLock {
            inner: Mutex::new(val),
        }
    }

    fn lock(&self) -> IrqSafeGuard<'_, T> {
        let preempt_was_enabled = crate::smp::preempt_is_enabled();
        if preempt_was_enabled {
            if let Some(pc) = crate::smp::try_current_per_cpu() {
                pc.preempt_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
            }
        }
        let was_enabled = crate::arch::CurrentArch::are_interrupts_enabled();
        if was_enabled {
            crate::arch::CurrentArch::disable_interrupts();
        }
        IrqSafeGuard {
            guard: Some(self.inner.lock()),
            was_enabled,
            preempt_was_enabled,
        }
    }
}

struct IrqSafeGuard<'a, T> {
    guard: Option<spin::MutexGuard<'a, T>>,
    was_enabled: bool,
    preempt_was_enabled: bool,
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
        if self.preempt_was_enabled {
            if let Some(pc) = crate::smp::try_current_per_cpu() {
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
                let prev = pc.preempt_count.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
                debug_assert!(prev > 0);
                #[cfg(target_arch = "x86_64")]
                if prev == 1 && pc.need_resched.load(Ordering::Relaxed) && pc.sched_active.load(Ordering::Acquire) {
                    if pc.need_resched.swap(false, Ordering::Relaxed) {
                        crate::task::maybe_resched_from_preempt();
                    }
                }
            }
        }
    }
}

// ── Exported types ────────────────────────────────────────────────

pub type TimerCallback = fn(context: *mut u8);

/// Identifies a pending timer.  `cpu` is the base the timer currently lives
/// on, so `cancel`/`migrate` route in O(1) without searching other bases.
/// A timer that has been migrated carries a fresh id; the old id is invalid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimerId {
    pub cpu: u32,
    pub seq: u64,
}

// ── UniversalTimer trait ──────────────────────────────────────────

pub trait UniversalTimer: Send + Sync {
    /// Fire `callback(context)` at or after `deadline_ns` (absolute),
    /// pinned to the calling CPU's base.
    fn set(&self, deadline_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId;

    /// Fire `callback(context)` every `interval_ns`, starting now, pinned
    /// to the calling CPU's base.
    fn set_periodic(&self, interval_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId;

    /// Cancel a pending timer.  Returns `false` if the timer already fired
    /// or was never registered.
    fn cancel(&self, id: TimerId) -> bool;

    /// Move a pending timer onto `target_cpu`'s base, returning a fresh
    /// `TimerId` for the relocated entry.  The old id is invalidated.
    /// Returns `None` if the timer already fired, was migrated, or never
    /// existed.
    fn migrate(&self, id: TimerId, target_cpu: u32) -> Option<TimerId>;

    /// Current monotonic time in nanoseconds.
    fn now_ns(&self) -> u64;
}

// ── Implementation ────────────────────────────────────────────────
//
// Each CPU owns a `TimerBase`: an IRQ-safe queue plus the LAPIC/SBI timer
// it drives.  Every queue mutation runs under that base's `IrqSafeLock`
// (spin + local IRQ disable), so there are no data races.  A base's
// hardware timer is always armed for its queue's earliest deadline:
//   - local set/cancel re-arm on earliest-change;
//   - remote set/migrate send a reschedule IPI when the earliest decreases;
//   - tick() always re-arms after draining.
// Earliest-increase (expire / remote cancel) needs no IPI: the hardware
// timer is still armed for the old (now-gone) earliest, fires, and re-arms
// from the actual queue — no deadline is ever lost, just re-timed.  The
// reschedule IPI is therefore a hint, never a correctness requirement.

struct TimerBase {
    queue: IrqSafeLock<TimerQueue>,
}

pub struct UniversalTimerImpl {
    clocksource: &'static dyn Clocksource,
    clockevent: &'static dyn Clockevent,
    bases: [TimerBase; crate::smp::MAX_CPUS],
    next_seq: AtomicU64,
}

impl UniversalTimerImpl {
    pub fn new(clocksource: &'static dyn Clocksource, clockevent: &'static dyn Clockevent) -> Self {
        UniversalTimerImpl {
            clocksource,
            clockevent,
            bases: core::array::from_fn(|_| TimerBase {
                queue: IrqSafeLock::new(TimerQueue::new()),
            }),
            next_seq: AtomicU64::new(1),
        }
    }

    /// Process expired timers on the *current* CPU's base and reprogram its
    /// clockevent.
    ///
    /// Called from the timer ISR or the reschedule IPI handler (interrupts
    /// disabled).  Runs on the CPU whose base is being processed, so the
    /// clockevent call below programs that CPU's own LAPIC/SBI timer.
    pub fn tick(&self) {
        let now = self.clocksource.now_ns();
        let cpu = crate::smp::current_cpu_id() as usize;
        let mut queue = self.bases[cpu].queue.lock();

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
        drop(queue);
        // Deadline expiry is a reschedule hint: flag the local CPU so the
        // next `schedule()` consumes it (`take_need_resched`). Atomics only —
        // never scheduler locks (SCHED-L002 ISR touch-nothing).
        crate::smp::set_need_resched();
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
        let cpu = crate::smp::current_cpu_id();
        let id = TimerId {
            cpu,
            seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
        };
        let mut queue = self.bases[cpu as usize].queue.lock();
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
        let cpu = crate::smp::current_cpu_id();
        let id = TimerId {
            cpu,
            seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
        };
        let mut queue = self.bases[cpu as usize].queue.lock();
        let old_next = queue.next_deadline();
        queue.insert(TimerEntry::new(
            id,
            deadline_ns,
            Some(interval_ns),
            callback,
            context,
        ));
        if old_next != queue.next_deadline() {
            self.reprogram(&mut queue);
        }
        id
    }

    fn cancel(&self, id: TimerId) -> bool {
        let mut queue = self.bases[id.cpu as usize].queue.lock();
        let old_next = queue.next_deadline();
        let removed = queue.cancel(id);
        // Re-arm locally only if we own the base.  A remote cancel never
        // re-arms: the owner's timer is still armed for the (removed)
        // earliest and re-arms itself on the next tick.
        if removed && old_next != queue.next_deadline() && id.cpu == crate::smp::current_cpu_id() {
            self.reprogram(&mut queue);
        }
        removed
    }

    fn migrate(&self, id: TimerId, target_cpu: u32) -> Option<TimerId> {
        // Remove from the source base first, dropping the lock before the
        // target base is touched — never hold two base locks at once.  The
        // source's timer (armed for the removed deadline) is a hint: it will
        // fire and re-arm from the real queue if it ever mattered.
        let entry = {
            let mut queue = self.bases[id.cpu as usize].queue.lock();
            queue.remove(id)?
        };

        let new_id = TimerId {
            cpu: target_cpu,
            seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
        };
        let mut entry = entry;
        entry.id = new_id;

        let mut queue = self.bases[target_cpu as usize].queue.lock();
        let old_next = queue.next_deadline();
        queue.insert(entry);
        if old_next != queue.next_deadline() {
            if target_cpu == crate::smp::current_cpu_id() {
                self.reprogram(&mut queue);
            } else {
                send_reschedule_ipi(target_cpu);
            }
        }
        Some(new_id)
    }

    fn now_ns(&self) -> u64 {
        self.clocksource.now_ns()
    }
}

// ── Reschedule IPI (cross-CPU reprogramming hint) ─────────────────

/// Ask `cpu` to re-arm its clockevent by running `tick()` on its own base.
///
/// Only sent when the target's earliest deadline moved earlier; a lost or
/// delayed IPI only re-times work, never loses it.
#[cfg(target_arch = "x86_64")]
fn send_reschedule_ipi(cpu: u32) {
    // cpu -> APIC id translation (cpu ids and APIC ids do not coincide).
    let apic_id = crate::smp::per_cpu_by_id(cpu).apic_id;
    crate::platform::x86_64_pc::apic::send_ipi(
        apic_id,
        crate::platform::x86_64_pc::apic::IPI_TIMER,
    );
}

#[cfg(target_arch = "riscv64")]
fn send_reschedule_ipi(cpu: u32) {
    // Matches the existing RiscvCpu convention: cpu_id == hart_id on QEMU
    // riscv-virt.
    crate::arch::riscv64::sbi::send_ipi(1u64 << cpu);
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
    // Wire the reschedule IPI handler (APIC vector 243) on x86_64.  On
    // riscv64 the SBI software-interrupt branch in trap.rs calls tick()
    // directly.
    #[cfg(target_arch = "x86_64")]
    crate::arch::x86_64::idt::set_timer_ipi_callback(universal_timer_ipi_tick);
}

/// C function called from the timer ISR.  Dispatches to the singleton.
///
/// x86_64 only: riscv64 drives `tick()` straight from its trap handler.
#[cfg(target_arch = "x86_64")]
fn universal_timer_tick() {
    let ut = universal_timer_impl();
    ut.tick();
}

/// C function called from the reschedule IPI handler.  Identical to the
/// timer tick — it re-processes the current CPU's base and re-arms.
#[cfg(target_arch = "x86_64")]
fn universal_timer_ipi_tick() {
    let ut = universal_timer_impl();
    ut.tick();
}

/// Return the global universal timer singleton.
///
/// Panics if `early_init()` has not been called.
pub fn universal_timer() -> &'static dyn UniversalTimer {
    *UNIVERSAL_TIMER
        .get()
        .expect("UniversalTimer not initialised — call early_init() first")
}

/// True once `early_init()` has run and the singleton is usable.
pub fn is_ready() -> bool {
    UNIVERSAL_TIMER.get().is_some()
}

/// Convenience: return the raw impl pointer for the ISR tick handler.
pub fn universal_timer_impl() -> &'static UniversalTimerImpl {
    *UNIVERSAL_TIMER
        .get()
        .expect("UniversalTimer not initialised")
}

// ── Blocking waits ────────────────────────────────────────────────
//
// These register a one-shot periodic on the *waiter's own* base, then HLT
// until it fires.  The waiter's LAPIC/SBI timer fires, its own ISR processes
// its own base and stores `true` into the per-waiter wake flag, and the
// waiter wakes — there is no cross-CPU wake dependency.  Each waiter uses a
// stack-local flag, so concurrent waiters on different CPUs never share
// state.

fn wake_callback(context: *mut u8) {
    unsafe {
        (*(context as *const AtomicBool)).store(true, Ordering::SeqCst);
    }
}

/// Wait until `done()` returns true, or `deadline_ns` (absolute) passes.
///
/// Arms a one-shot timer at the deadline (or a coarse re-check cadence,
/// whichever is sooner), then HLTs.  Any interrupt (device IRQ, IPI, or the
/// timer itself) wakes the CPU and `done()` is re-evaluated, so an
/// interrupt-driven completion is serviced at interrupt latency — not on a
/// fixed 1 kHz poll.  The one-shot bounds the sleep even when no interrupt is
/// ever raised; the coarse re-check (re-armed only when the timer fires,
/// ~10 ms) keeps pure register-poll waits (port reset, controller start)
/// progressing when the hardware generates no interrupt.  Returns `true` if
/// `done()` became true before the deadline.
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

    // One-shot wake, re-armed only when it fires (not on every device IRQ),
    // so a wait burns at most one interrupt per fallback interval.  Pinned to
    // this CPU's own base, so this CPU's own ISR processes it and re-arms the
    // clockevent.
    let wake = AtomicBool::new(false);
    loop {
        if done() {
            return true;
        }
        let now = universal_timer().now_ns();
        if now >= deadline_ns {
            // One last chance — the condition may have just become true.
            return done();
        }
        let id = universal_timer().set(
            now.saturating_add(POLL_FALLBACK_NS).min(deadline_ns),
            wake_callback,
            &wake as *const AtomicBool as *mut u8,
        );
        wake.store(false, Ordering::SeqCst);
        loop {
            // HLT returns on ANY interrupt — device IRQ, IPI, or the timer.
            // Re-evaluate `done()` immediately so an interrupt-driven
            // completion is serviced at interrupt latency, not on the timer
            // cadence.  The timer interrupt additionally breaks us out to
            // re-arm for the next fallback window.
            crate::arch::CurrentArch::halt();
            if done() {
                universal_timer().cancel(id);
                return true;
            }
            if universal_timer().now_ns() >= deadline_ns {
                universal_timer().cancel(id);
                return done();
            }
            if wake.load(Ordering::SeqCst) {
                break;
            }
        }
        universal_timer().cancel(id);
    }
}

/// Coarse re-check cadence for [`wait_until_cond`] when the hardware never
/// raises an interrupt for the condition being polled.  Old code woke every
/// 1 ms; this is 10 ms — a 10× cut in idle timer interrupts while still
/// bounding pure register-poll waits to ~10 ms detection latency.
const POLL_FALLBACK_NS: u64 = 10_000_000;

/// Cooperative sibling of [`wait_until_cond`]: when a task context exists
/// (the audio pump parks as a scheduler task), park the current task in
/// `slice_ns` slices instead of HLTing the CPU, so the rest of the system
/// keeps flowing while the audio DMA runs on its own.  `done()` is
/// re-evaluated every slice — a completion is serviced within one slice
/// without depending on an ISR wake.  Falls back to [`wait_until_cond`] in
/// boot context (no current task), where there is nothing to schedule and
/// HLT is correct.
pub fn wait_until_cond_coop(deadline_ns: u64, slice_ns: u64, done: &dyn Fn() -> bool) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        if crate::smp::current_per_cpu().current_task.load(core::sync::atomic::Ordering::Relaxed).is_null() {
            return wait_until_cond(deadline_ns, done);
        }
        loop {
            if done() {
                return true;
            }
            let now = universal_timer().now_ns();
            if now >= deadline_ns {
                // One last chance — the condition may have just become true.
                return done();
            }
            let slice = slice_ns.min(deadline_ns - now).max(1);
            crate::task::sleep_current(slice);
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = slice_ns;
        return wait_until_cond(deadline_ns, done);
    }
}

/// Block until `now_ns() >= deadline_ns` (absolute), yielding the CPU.
///
/// A pure sleep — unlike [`wait_until_cond`] there is no condition to poll, so
/// this arms a single one-shot at the deadline and HLTs until it fires.  The
/// idle loop relies on this to park between sleeper deadlines while burning
/// exactly one timer interrupt per wait (not one per millisecond).
pub fn wait_until(deadline_ns: u64) {
    if !crate::arch::CurrentArch::are_interrupts_enabled() {
        loop {
            if universal_timer().now_ns() >= deadline_ns {
                return;
            }
            core::hint::spin_loop();
        }
    }

    let wake = AtomicBool::new(false);
    let id = universal_timer().set(
        deadline_ns,
        wake_callback,
        &wake as *const AtomicBool as *mut u8,
    );
    while !wake.load(Ordering::SeqCst) {
        crate::arch::CurrentArch::halt();
    }
    universal_timer().cancel(id);
}

/// Block for `ms` milliseconds, yielding the CPU.
pub fn sleep_ms(ms: u64) {
    wait_until(universal_timer().now_ns() + ms * 1_000_000);
}

/// Block for `ns` nanoseconds, yielding the CPU.
pub fn sleep_ns(ns: u64) {
    wait_until(universal_timer().now_ns() + ns);
}

/// Current monotonic time in nanoseconds.
pub fn now_ns() -> u64 {
    universal_timer().now_ns()
}

/// Arm a one-shot entry on the current CPU's base.  Used by the scheduler's
/// slice timer so sleep deadlines and slice expiry share a single LAPIC
/// arming owner (UniversalTimer).  `None` before `early_init`.
pub fn set_oneshot(
    deadline_ns: u64,
    callback: TimerCallback,
    context: *mut u8,
) -> Option<TimerId> {
    let ut = *UNIVERSAL_TIMER.get()?;
    Some(ut.set(deadline_ns, callback, context))
}

/// Cancel a previously armed entry (any CPU's base).  `false` before
/// `early_init` or if the entry already fired.
pub fn cancel_timer_id(id: TimerId) -> bool {
    match UNIVERSAL_TIMER.get() {
        Some(&ut) => ut.cancel(id),
        None => false,
    }
}
