use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Once;

use super::clockevent::Clockevent;
use super::clocksource::Clocksource;
use super::irqsafe::IrqLock;
use super::lockorder;
use super::timer_queue::{TimerEntry, TimerQueue};

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
// it drives.  Every queue mutation runs under that base's `IrqLock` (spin +
// local IRQ disable), so there are no data races.  A base's hardware timer
// is always armed for its queue's earliest deadline:
//   - local set/cancel re-arm on earliest-change;
//   - remote set/migrate send a reschedule IPI when the earliest decreases;
//   - tick() always re-arms after draining.
// Earliest-increase (expire / remote cancel) needs no IPI: the hardware
// timer is still armed for the old (now-gone) earliest, fires, and re-arms
// from the actual queue — no deadline is ever lost, just re-timed.  The
// reschedule IPI is therefore a hint, never a correctness requirement.

struct TimerBase {
    queue: IrqLock<TimerQueue>,
}

pub struct UniversalTimerImpl {
    clocksource: &'static dyn Clocksource,
    clockevent: &'static dyn Clockevent,
    bases: [TimerBase; crate::smp::MAX_CPUS],
    next_seq: AtomicU64,
}

impl UniversalTimerImpl {
    pub fn new(
        clocksource: &'static dyn Clocksource,
        clockevent: &'static dyn Clockevent,
    ) -> Self {
        UniversalTimerImpl {
            clocksource,
            clockevent,
            bases: core::array::from_fn(|_| TimerBase {
                queue: IrqLock::with_order(TimerQueue::new(), lockorder::TIMER_QUEUE),
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
    }

    /// Program the clockevent of the *current* CPU to the earliest pending
    /// deadline on that CPU's base.
    fn reprogram(&self, queue: &mut TimerQueue) {
        if let Some(next) = queue.next_deadline() {
            self.clockevent.set_deadline(next);
        } else {
            self.clockevent.stop();
        }
    }

    /// Remove a pending timer by id and return its opaque context pointer, or
    /// `None` if it already fired, was migrated, or never existed.
    ///
    /// Mirrors `cancel`, but returns the entry's `context` instead of a bool so
    /// the caller can reclaim the context (e.g. `Box::from_raw` + drop) when a
    /// sleeping task is killed or exits before its deadline. Re-arms the local
    /// clockevent if the earliest pending deadline changed.
    pub fn remove_context(&self, id: TimerId) -> Option<*mut u8> {
        let mut queue = self.bases[id.cpu as usize].queue.lock();
        let old_next = queue.next_deadline();
        let removed = queue.remove(id);
        if removed.is_some()
            && old_next != queue.next_deadline()
            && id.cpu == crate::smp::current_cpu_id()
        {
            self.reprogram(&mut queue);
        }
        removed.map(|e| e.context)
    }
}

impl UniversalTimer for UniversalTimerImpl {
    fn set(&self, deadline_ns: u64, callback: TimerCallback, context: *mut u8) -> TimerId {
        let cpu = crate::smp::current_cpu_id();
        let id = TimerId { cpu, seq: self.next_seq.fetch_add(1, Ordering::Relaxed) };
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
        let id = TimerId { cpu, seq: self.next_seq.fetch_add(1, Ordering::Relaxed) };
        let mut queue = self.bases[cpu as usize].queue.lock();
        let old_next = queue.next_deadline();
        queue.insert(TimerEntry::new(id, deadline_ns, Some(interval_ns), callback, context));
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
        if removed && old_next != queue.next_deadline() && id.cpu == crate::smp::current_cpu_id()
        {
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

        let new_id = TimerId { cpu: target_cpu, seq: self.next_seq.fetch_add(1, Ordering::Relaxed) };
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
    crate::platform::x86_64_pc::apic::send_ipi(apic_id, crate::platform::x86_64_pc::apic::IPI_TIMER);
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
    // Wire the reschedule IPI handler (APIC vector 52) on x86_64.  On
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
    // cadence even if the hardware never raises an interrupt.  Pinned to
    // this CPU's own base, so this CPU's own ISR processes and wakes us.
    let wake = AtomicBool::new(false);
    let id = universal_timer().set_periodic(
        1_000_000,
        wake_callback,
        &wake as *const AtomicBool as *mut u8,
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
        wake.store(false, Ordering::SeqCst);
        while !wake.load(Ordering::SeqCst) {
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
