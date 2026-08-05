use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::riscv64::sbi;
use crate::arch::riscv64::time;
use crate::services::clockevent::Clockevent;
use crate::services::clocksource::Clocksource;

const DEFAULT_TIMEBASE_HZ: u64 = 10_000_000;

/// CPU timebase frequency in Hz, used to scale `time` CSR ticks to ns.
///
/// Populated from the DTB `/cpus` `timebase-frequency` property during arch
/// init.  Falls back to 10 MHz (the QEMU riscv-virt default) when the DTB
/// is absent or does not advertise one.
static TIMEBASE_HZ: AtomicU64 = AtomicU64::new(DEFAULT_TIMEBASE_HZ);

/// Override the CPU timebase frequency (Hz).
///
/// Ignored when `hz == 0`, leaving the default in place.
pub fn set_timebase_hz(hz: u64) {
    if hz != 0 {
        TIMEBASE_HZ.store(hz, Ordering::Relaxed);
    }
}

/// Current CPU timebase frequency in Hz.
pub fn timebase_hz() -> u64 {
    TIMEBASE_HZ.load(Ordering::Relaxed)
}

/// Convert a `time` CSR tick count to nanoseconds.
fn ticks_to_ns(ticks: u64) -> u64 {
    let hz = timebase_hz();
    if hz == 0 {
        return 0;
    }
    // (ticks / hz) * 1e9 + ((ticks % hz) * 1e9) / hz — avoids overflow.
    (ticks / hz) * 1_000_000_000 + ((ticks % hz) * 1_000_000_000) / hz
}

/// Convert an absolute nanosecond deadline to `time` CSR ticks.
fn ns_to_ticks(deadline_ns: u64) -> u64 {
    let hz = timebase_hz();
    if hz == 0 {
        return 0;
    }
    // deadline_ns * hz / 1e9, split to avoid overflow on large deadlines.
    if deadline_ns >= 1_000_000_000 {
        (deadline_ns / 1_000_000_000) * hz + ((deadline_ns % 1_000_000_000) * hz) / 1_000_000_000
    } else {
        (deadline_ns * hz) / 1_000_000_000
    }
}

// ── RISC-V `time` CSR clocksource ─────────────────────────────────

pub struct RiscvTimebaseClocksource;

impl Clocksource for RiscvTimebaseClocksource {
    fn now_ns(&self) -> u64 {
        ticks_to_ns(time::read_time())
    }
}

// ── RISC-V SBI timer clockevent ───────────────────────────────────

/// Far-future deadline (in ticks) used as an effective "stop" — SBI has no
/// cancel-timer call, so we just program a deadline that will never be hit.
const STOP_FALLBACK_TICKS: u64 = 1 << 60;

pub struct RiscvSbiClockevent;

impl Clockevent for RiscvSbiClockevent {
    /// Program the SBI timer to fire at (or slightly after) `deadline_ns`.
    ///
    /// `deadline_ns` is absolute (matching the clocksource), converted to
    /// absolute `time` CSR ticks via the same timebase.  Deadlines already
    /// in the past fire immediately.
    fn set_deadline(&self, deadline_ns: u64) {
        let ticks = ns_to_ticks(deadline_ns);
        sbi::set_timer(core::cmp::max(ticks, time::read_time()));
    }

    /// Effectively stop the timer by programming a far-future deadline.
    fn stop(&self) {
        sbi::set_timer(time::read_time().saturating_add(STOP_FALLBACK_TICKS));
    }
}
