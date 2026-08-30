//! CPU slow-mode for the kernel — delegates to the shared guarded
//! implementation in `common` so it cannot drift from the bootloader's copy.
//!
//! When `cpu_slow` is enabled each CPU programs the Intel HWP / EIST /
//! clock-modulation MSRs for ~800 MHz immediately at boot. Firmware or later
//! power-policy writes may overwrite those MSRs, so this module also offers a
//! 100 ms periodic re-application on each CPU's own `UniversalTimer` base.
//! The repeat is opt-out via the bootarg `nocpuslowrepeat` (also
//! `-nocpuslowrepeat` / `--nocpuslowrepeat`).

pub use common::cpu_slow::enable_cpu_slow_mode;

#[cfg(feature = "cpu_slow")]
const REPEAT_INTERVAL_NS: u64 = 10_000_000; // 100 ms

#[cfg(feature = "cpu_slow")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "cpu_slow")]
static REPEAT_ARMED: [AtomicBool; 16] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

#[cfg(feature = "cpu_slow")]
static REPEAT_NOTIFIED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "cpu_slow")]
fn cpu_slow_repeat_tick(_ctx: *mut u8) {
    // SAFETY: MSR / CPUID probing is the same guarded sequence as the
    // one-shot `enable_cpu_slow_mode`; it touches only per-CPU MSRs and
    // never takes locks, so it is safe to run in timer ISR context while
    // the `UniversalTimer` queue lock is held (interrupts disabled).
    unsafe { enable_cpu_slow_mode(); }
}

/// Arm the 100 ms periodic `cpu_slow` re-application for the calling CPU.
///
/// No-op when:
/// - the `cpu_slow` feature is not enabled (compile-time),
/// - `nocpuslowrepeat` is present on the kernel command line (opt-out),
/// - the `UniversalTimer` has not been initialised yet, or
/// - this CPU already has the repeat armed (idempotent).
///
/// The timer is pinned to the calling CPU's `UniversalTimer` base via
/// `set_periodic`, so each CPU must call this on itself (BSP in
/// `Kernel::init`, APs in `ap_entry64`). The ISR callback re-invokes
/// `enable_cpu_slow_mode` and is intentionally lock-free and allocation-free.
/// Only the first successful arm (BSP) emits a single serial line
/// `"[cpu_slow] 100ms repeat enabled"` so the feature is visible without
/// spamming per-CPU.
#[cfg(feature = "cpu_slow")]
pub fn arm_repeat() {
    if crate::bootargs::is_nocpuslowrepeat() {
        // Opt-out: do not arm, but still need per-CPU gate so we don't spin.
        // Log once globally rather than per CPU.
        if !REPEAT_NOTIFIED.swap(true, Ordering::SeqCst) {
            crate::drivers::serial::SerialPort::puts("[cpu_slow] 100ms repeat disabled via nocpuslowrepeat\n");
        }
        // Mark this CPU as handled so future re-entries are no-ops without extra checks.
        if let Some(pc) = crate::smp::try_current_per_cpu() {
            let cpu = pc.cpu_id as usize;
            if cpu < REPEAT_ARMED.len() {
                REPEAT_ARMED[cpu].store(true, Ordering::Relaxed);
            }
        }
        return;
    }
    if !crate::services::universal_timer::is_ready() {
        return;
    }
    let cpu = crate::smp::try_current_per_cpu()
        .map(|pc| pc.cpu_id as usize)
        .unwrap_or(0);
    if cpu >= REPEAT_ARMED.len() {
        return;
    }
    // Idempotent: only the first caller per CPU arms.
    if REPEAT_ARMED[cpu].swap(true, Ordering::SeqCst) {
        return;
    }
    let _id = crate::services::universal_timer::universal_timer()
        .set_periodic(REPEAT_INTERVAL_NS, cpu_slow_repeat_tick, core::ptr::null_mut());
    // Single global notification, not per CPU.
    if !REPEAT_NOTIFIED.swap(true, Ordering::SeqCst) {
        crate::drivers::serial::SerialPort::puts("[cpu_slow] 100ms repeat enabled (opt-out with nocpuslowrepeat)\n");
    }
}

#[cfg(feature = "cpu_slow")]
#[deprecated(note = "renamed to arm_repeat")]
pub fn maybe_arm_repeat() {
    arm_repeat()
}

#[cfg(not(feature = "cpu_slow"))]
pub fn arm_repeat() {
    // No-op when the feature is off — allows unconditional caller sites if desired.
}

#[cfg(not(feature = "cpu_slow"))]
pub fn maybe_arm_repeat() {
    // No-op alias.
}
