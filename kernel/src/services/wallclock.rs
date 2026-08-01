//! Wall-clock time service.
//!
//! On x86_64 the CMOS RTC (ports 0x70/0x71) provides a battery-backed wall
//! clock; [`now_epoch_secs`] returns seconds since the Unix epoch. On riscv64
//! there is no RTC, so the service reports `None` and callers fall back to the
//! monotonic timer.

/// Seconds since the Unix epoch, if a wall clock is available.
#[cfg(target_arch = "x86_64")]
pub fn now_epoch_secs() -> Option<u64> {
    crate::drivers::rtc::read_epoch_secs()
}

/// Seconds since the Unix epoch, if a wall clock is available.
#[cfg(target_arch = "riscv64")]
pub fn now_epoch_secs() -> Option<u64> {
    None
}

/// Seconds since the Unix epoch when a wall clock exists; otherwise seconds
/// since boot (monotonic).  A non-zero, monotonic modification-time proxy
/// usable on any architecture (tmpfs runs on riscv64, which has no RTC).
pub fn now_secs() -> u64 {
    now_epoch_secs().unwrap_or_else(|| crate::services::universal_timer::now_ns() / 1_000_000_000)
}

/// Days since 1970-01-01 for a civil (proleptic Gregorian) date.
/// Howard Hinnant's `days_from_civil` algorithm.
pub fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: civil (y, m, d) for a day number.
pub fn civil_from_days(z: u64) -> (u64, u64, u64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
