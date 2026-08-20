//! `<time.h>` — wall clock, monotonic timer, and sleep.
//!
//! `time()/gettimeofday()/clock_gettime(CLOCK_REALTIME)` come from
//! `/kernel/timer:epoch_secs` (wallclock → CMOS RTC, falling back to boot
//! seconds); `CLOCK_MONOTONIC` from `read(/kernel/timer)`.  Sleeps route to
//! `/kernel/timer:sleep*`.  No `tzset`/calendar conversion beyond what the
//! kernel offers.

use core::ffi::{c_int, c_long, c_uint, c_void};

use crate::errno;
use crate::syscall::{read_path, write_path};

pub const CLOCKS_PER_SEC: u64 = 1_000_000;
pub const CLOCK_REALTIME: c_int = 0;
pub const CLOCK_MONOTONIC: c_int = 1;

/// `struct timespec`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// `struct timeval`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// Seconds since the Unix epoch (wall clock; falls back to boot seconds).
fn epoch_secs() -> isize {
    let mut buf = [0u8; 8];
    // Pass the full buffer length so the kernel can copy the 8-byte result
    // back into it; the write syscall's output is bounded by this length.
    let r = unsafe { write_path(b"/kernel/timer:epoch_secs\0", &mut buf, 8, 0) };
    if r < 8 {
        return -1;
    }
    let s = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    errno::ret(s as isize)
}

/// Monotonic nanoseconds since boot.
fn now_ns() -> isize {
    let mut buf = [0u8; 8];
    let r = unsafe { read_path(b"/kernel/timer\0", &mut buf, 0) };
    if r < 8 {
        return -1;
    }
    let ns = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    errno::ret(ns as isize)
}

/// POSIX `time(&tloc)`: seconds since the epoch, or (time_t)-1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn time(tloc: *mut c_long) -> c_long {
    let s = epoch_secs();
    if s >= 0 && !tloc.is_null() {
        unsafe {
            *tloc = s as c_long;
        }
    }
    s as c_long
}

/// POSIX `gettimeofday(&tv, tz)`: seconds + microseconds.  `tz` is ignored.
#[unsafe(no_mangle)]
pub extern "C" fn gettimeofday(tv: *mut Timeval, tz: *mut c_void) -> c_int {
    let _ = tz;
    if tv.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    let s = epoch_secs();
    if s < 0 {
        return -1;
    }
    let ns = now_ns();
    let usec = if ns >= 0 {
        (ns as u64 % 1_000_000_000) / 1000
    } else {
        0
    };
    unsafe {
        (*tv).tv_sec = s as i64;
        (*tv).tv_usec = usec as i64;
    }
    0
}

/// POSIX `clock_gettime(clockid, &ts)`.
#[unsafe(no_mangle)]
pub extern "C" fn clock_gettime(clockid: c_int, ts: *mut Timespec) -> c_int {
    if ts.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    match clockid {
        CLOCK_REALTIME => {
            let s = epoch_secs();
            if s < 0 {
                return -1;
            }
            let ns = now_ns();
            let nsec = if ns >= 0 {
                (ns as u64 % 1_000_000_000) as i64
            } else {
                0
            };
            unsafe {
                (*ts).tv_sec = s as i64;
                (*ts).tv_nsec = nsec;
            }
            0
        }
        CLOCK_MONOTONIC => {
            let ns = now_ns();
            if ns < 0 {
                return -1;
            }
            unsafe {
                (*ts).tv_sec = (ns as u64 / 1_000_000_000) as i64;
                (*ts).tv_nsec = (ns as u64 % 1_000_000_000) as i64;
            }
            0
        }
        _ => {
            errno::set(errno::EINVAL);
            -1
        }
    }
}

/// POSIX `nanosleep(&req, &rem)`: block for the requested duration.
/// `rem` is left untouched (we never return early).
#[unsafe(no_mangle)]
pub extern "C" fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int {
    let _ = rem;
    if req.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    let sec = unsafe { (*req).tv_sec };
    let nsec = unsafe { (*req).tv_nsec };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        errno::set(errno::EINVAL);
        return -1;
    }
    let total = (sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64);
    let r = crate::process::sleep_ns(total);
    if r < 0 { -1 } else { 0 }
}

/// POSIX `sleep(secs)`: returns the unslept seconds (0 on success).
#[unsafe(no_mangle)]
pub extern "C" fn sleep(secs: c_uint) -> c_uint {
    let r = crate::process::sleep(secs as u64);
    if r < 0 { secs } else { 0 }
}

/// POSIX `usleep(usecs)`: returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn usleep(usecs: c_uint) -> c_int {
    let r = crate::process::usleep(usecs as u64);
    if r < 0 { -1 } else { 0 }
}

/// `clock()`: approximate CPU time as CLOCKS_PER_SEC ticks (µs since boot).
#[unsafe(no_mangle)]
pub extern "C" fn clock() -> c_long {
    let ns = now_ns();
    if ns < 0 {
        (-1) as c_long
    } else {
        (ns as u64 / (1_000_000_000 / CLOCKS_PER_SEC)) as c_long
    }
}
