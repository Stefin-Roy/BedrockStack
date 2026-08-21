//! `<time.h>` — wall clock, monotonic timer, and sleep.
//!
//! `time()/gettimeofday()/clock_gettime(CLOCK_REALTIME)` come from
//! `/kernel/timer:epoch_secs` (wallclock → CMOS RTC, falling back to boot
//! seconds); `CLOCK_MONOTONIC` from `read(/kernel/timer)`.  Sleeps route to
//! `/kernel/timer:sleep*`.  No `tzset`/calendar conversion beyond what the
//! kernel offers.

use core::ffi::{c_char, c_int, c_long, c_uint, c_void};

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

#[unsafe(no_mangle)]
pub extern "C" fn clock_getres(clockid: c_int, res: *mut Timespec) -> c_int {
    if res.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC => {
            unsafe { *res = Timespec { tv_sec: 0, tv_nsec: 1_000_000 } };
            0
        }
        _ => {
            errno::set(errno::EINVAL);
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn difftime(t1: c_long, t0: c_long) -> f64 {
    (t1 - t0) as f64
}

/// Minimal `tm` for gmtime/mktime conversions (UTC only, C locale).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Tm {
    pub tm_sec: c_int,
    pub tm_min: c_int,
    pub tm_hour: c_int,
    pub tm_mday: c_int,
    pub tm_mon: c_int,
    pub tm_year: c_int,
    pub tm_wday: c_int,
    pub tm_yday: c_int,
    pub tm_isdst: c_int,
    pub tm_gmtoff: c_long,
    pub tm_zone: *const c_char,
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}
fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap(y) { 29 } else { 28 },
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gmtime_r(t: *const c_long, result: *mut Tm) -> *mut Tm {
    if t.is_null() || result.is_null() {
        return core::ptr::null_mut();
    }
    let mut secs = unsafe { *t as i64 };
    // Use simple algorithm: days since 1970-01-01.
    let days = secs.div_euclid(86400);
    let mut rem = secs.rem_euclid(86400);
    let sec = (rem % 60) as c_int;
    rem /= 60;
    let min = (rem % 60) as c_int;
    let hour = (rem / 60) as c_int;
    let mut y = 1970;
    let mut d = days;
    loop {
        let yd = if is_leap(y) { 366 } else { 365 };
        if d < yd as i64 { break; }
        d -= yd as i64;
        y += 1;
    }
    let yday = d as c_int;
    let mut mon = 0;
    while mon < 12 {
        let dim = days_in_month(y, (mon + 1) as u32) as i64;
        if d < dim { break; }
        d -= dim;
        mon += 1;
    }
    let mday = (d + 1) as c_int;
    // wday: 1970-01-01 was Thursday (4)
    let wday = ((days + 4).rem_euclid(7)) as c_int;
    unsafe {
        (*result).tm_sec = sec;
        (*result).tm_min = min;
        (*result).tm_hour = hour;
        (*result).tm_mday = mday;
        (*result).tm_mon = mon as c_int;
        (*result).tm_year = y - 1900;
        (*result).tm_wday = wday;
        (*result).tm_yday = yday;
        (*result).tm_isdst = 0;
        (*result).tm_gmtoff = 0;
        (*result).tm_zone = b"UTC\0".as_ptr() as *const c_char;
    }
    result
}

static mut GMTIME_BUF: Tm = Tm { tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 1, tm_mon: 0, tm_year: 70, tm_wday: 4, tm_yday: 0, tm_isdst: 0, tm_gmtoff: 0, tm_zone: core::ptr::null() };

#[unsafe(no_mangle)]
pub extern "C" fn gmtime(t: *const c_long) -> *mut Tm {
    unsafe { gmtime_r(t, core::ptr::addr_of_mut!(GMTIME_BUF)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn localtime_r(t: *const c_long, result: *mut Tm) -> *mut Tm {
    gmtime_r(t, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn localtime(t: *const c_long) -> *mut Tm {
    gmtime(t)
}

#[unsafe(no_mangle)]
pub extern "C" fn mktime(tm: *mut Tm) -> c_long {
    if tm.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        let y = (*tm).tm_year + 1900;
        let mon = (*tm).tm_mon as u32;
        let mday = (*tm).tm_mday as i64;
        let hour = (*tm).tm_hour as i64;
        let min = (*tm).tm_min as i64;
        let sec = (*tm).tm_sec as i64;
        // Days since epoch.
        let mut days: i64 = 0;
        for yr in 1970..y {
            days += if is_leap(yr) { 366 } else { 365 };
        }
        for m in 0..mon {
            days += days_in_month(y, m + 1) as i64;
        }
        days += mday - 1;
        let secs = days * 86400 + hour * 3600 + min * 60 + sec;
        (*tm).tm_wday = ((days + 4).rem_euclid(7)) as c_int;
        (*tm).tm_yday = (days - {
            let mut d = 0;
            for yr in 1970..y { d += if is_leap(yr) {366} else {365} }
            d
        }) as c_int;
        secs as c_long
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn asctime_r(tm: *const Tm, buf: *mut c_char) -> *mut c_char {
    if tm.is_null() || buf.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let t = &*tm;
        let wdays = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
        let mons = [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"];
        let w = if t.tm_wday >= 0 && t.tm_wday < 7 { wdays[t.tm_wday as usize] } else { b"???" };
        let m = if t.tm_mon >= 0 && t.tm_mon < 12 { mons[t.tm_mon as usize] } else { b"???" };
        // "Www Mmm dd hh:mm:ss yyyy\n\0" 26 bytes
        let s = format_args!("{} {} {:02} {:02}:{:02}:{:02} {}\n",
            core::str::from_utf8(w).unwrap_or("???"),
            core::str::from_utf8(m).unwrap_or("???"),
            t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec, t.tm_year + 1900);
        let mut tmp = [0u8; 64];
        let mut len = 0usize;
        let _ = core::fmt::Write::write_fmt(&mut AsctimeBuf { buf: &mut tmp, len: &mut len }, s);
        if len >= 64 { len = 63; }
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf as *mut u8, len);
        *buf.add(len) = 0;
        buf
    }
}
struct AsctimeBuf<'a> { buf: &'a mut [u8], len: &'a mut usize }
impl core::fmt::Write for AsctimeBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = core::cmp::min(s.len(), self.buf.len() - *self.len);
        self.buf[*self.len..*self.len + n].copy_from_slice(s.as_bytes());
        *self.len += n;
        Ok(())
    }
}
static mut ASC_BUF: [u8; 64] = [0; 64];
#[unsafe(no_mangle)]
pub extern "C" fn asctime(tm: *const Tm) -> *mut c_char {
    unsafe { asctime_r(tm, core::ptr::addr_of_mut!(ASC_BUF) as *mut c_char) }
}
static mut CTIME_BUF: [u8; 64] = [0; 64];
#[unsafe(no_mangle)]
pub extern "C" fn ctime(t: *const c_long) -> *mut c_char {
    let tm = gmtime(t);
    if tm.is_null() { return core::ptr::null_mut(); }
    unsafe { asctime_r(&*tm, core::ptr::addr_of_mut!(CTIME_BUF) as *mut c_char) }
}
#[unsafe(no_mangle)]
pub extern "C" fn ctime_r(t: *const c_long, buf: *mut c_char) -> *mut c_char {
    let tm = gmtime_r(t, unsafe { &mut *core::ptr::addr_of_mut!(GMTIME_BUF) });
    if tm.is_null() { return core::ptr::null_mut(); }
    asctime_r(tm, buf)
}

#[unsafe(no_mangle)]
pub extern "C" fn strftime(s: *mut c_char, max: usize, fmt: *const c_char, tm: *const Tm) -> usize {
    if s.is_null() || fmt.is_null() || tm.is_null() || max == 0 {
        return 0;
    }
    unsafe {
        let t = &*tm;
        let mut out = [0u8; 512];
        let mut len = 0usize;
        let flen = crate::string::strlen(fmt);
        let f = core::slice::from_raw_parts(fmt as *const u8, flen);
        let mut i = 0usize;
        while i < f.len() && len < out.len() - 1 {
            if f[i] != b'%' {
                out[len] = f[i];
                len += 1;
                i += 1;
                continue;
            }
            i += 1;
            if i >= f.len() { break; }
            let spec = f[i];
            i += 1;
            match spec {
                b'Y' => {
                    let y = (t.tm_year + 1900) as u32;
                    let mut b = [0u8; 16];
                    let n = format_u32(y, &mut b);
                    if len + n < out.len() {
                        out[len..len + n].copy_from_slice(&b[..n]);
                        len += n;
                    }
                }
                b'y' => {
                    let y = (t.tm_year + 1900) % 100;
                    let mut b = [0u8; 4];
                    b[0] = b'0' + (y / 10) as u8;
                    b[1] = b'0' + (y % 10) as u8;
                    if len + 2 < out.len() {
                        out[len..len + 2].copy_from_slice(&b[..2]);
                        len += 2;
                    }
                }
                b'm' => {
                    let m = t.tm_mon + 1;
                    let mut b = [0u8; 4];
                    b[0] = b'0' + (m / 10) as u8;
                    b[1] = b'0' + (m % 10) as u8;
                    if len + 2 < out.len() {
                        out[len..len + 2].copy_from_slice(&b[..2]);
                        len += 2;
                    }
                }
                b'd' => {
                    let d = t.tm_mday;
                    let mut b = [0u8; 4];
                    b[0] = b'0' + (d / 10) as u8;
                    b[1] = b'0' + (d % 10) as u8;
                    if len + 2 < out.len() {
                        out[len..len + 2].copy_from_slice(&b[..2]);
                        len += 2;
                    }
                }
                b'H' => {
                    let h = t.tm_hour;
                    let mut b = [0u8; 4];
                    b[0] = b'0' + (h / 10) as u8;
                    b[1] = b'0' + (h % 10) as u8;
                    if len + 2 < out.len() {
                        out[len..len + 2].copy_from_slice(&b[..2]);
                        len += 2;
                    }
                }
                b'M' => {
                    let m = t.tm_min;
                    let mut b = [0u8; 4];
                    b[0] = b'0' + (m / 10) as u8;
                    b[1] = b'0' + (m % 10) as u8;
                    if len + 2 < out.len() {
                        out[len..len + 2].copy_from_slice(&b[..2]);
                        len += 2;
                    }
                }
                b'S' => {
                    let s = t.tm_sec;
                    let mut b = [0u8; 4];
                    b[0] = b'0' + (s / 10) as u8;
                    b[1] = b'0' + (s % 10) as u8;
                    if len + 2 < out.len() {
                        out[len..len + 2].copy_from_slice(&b[..2]);
                        len += 2;
                    }
                }
                b'%' => {
                    if len + 1 < out.len() {
                        out[len] = b'%';
                        len += 1;
                    }
                }
                _ => {}
            }
        }
        let n = core::cmp::min(len, max - 1);
        core::ptr::copy_nonoverlapping(out.as_ptr(), s as *mut u8, n);
        *s.add(n) = 0;
        n
    }
}
fn format_u32(mut v: u32, buf: &mut [u8]) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0usize;
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
    }
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    n
}
#[unsafe(no_mangle)]
pub extern "C" fn strptime(s: *const c_char, fmt: *const c_char, tm: *mut Tm) -> *mut c_char {
    if s.is_null() || fmt.is_null() || tm.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        let mut si = 0usize;
        let slen = crate::string::strlen(s);
        let sbytes = core::slice::from_raw_parts(s as *const u8, slen);
        let flen = crate::string::strlen(fmt);
        let fbytes = core::slice::from_raw_parts(fmt as *const u8, flen);
        let mut fi = 0usize;
        // Initialize tm to zero? Keep existing but ensure valid.
        (*tm).tm_isdst = -1;
        while fi < fbytes.len() {
            if fbytes[fi] != b'%' {
                // Literal: skip whitespace handling? POSIX says whitespace in fmt matches zero or more whitespace in s.
                if fbytes[fi].is_ascii_whitespace() {
                    while si < sbytes.len() && sbytes[si].is_ascii_whitespace() { si += 1; }
                    while fi < fbytes.len() && fbytes[fi].is_ascii_whitespace() { fi += 1; }
                    continue;
                }
                if si >= sbytes.len() || sbytes[si] != fbytes[fi] {
                    return core::ptr::null_mut();
                }
                si += 1;
                fi += 1;
                continue;
            }
            fi += 1;
            if fi >= fbytes.len() { break; }
            let spec = fbytes[fi];
            fi += 1;
            match spec {
                b'Y' => {
                    let (v, n) = parse_num(sbytes, si, 4);
                    if n == 0 { return core::ptr::null_mut(); }
                    (*tm).tm_year = (v as i32) - 1900;
                    si += n;
                }
                b'y' => {
                    let (v, n) = parse_num(sbytes, si, 2);
                    if n == 0 { return core::ptr::null_mut(); }
                    let y = v as i32;
                    // POSIX: 00-68 -> 2000-2068, 69-99 -> 1969-1999
                    (*tm).tm_year = if y < 69 { y + 100 } else { y };
                    si += n;
                }
                b'm' => {
                    let (v, n) = parse_num(sbytes, si, 2);
                    if n == 0 || v < 1 || v > 12 { return core::ptr::null_mut(); }
                    (*tm).tm_mon = (v as i32) - 1;
                    si += n;
                }
                b'd' | b'e' => {
                    let (v, n) = parse_num(sbytes, si, 2);
                    if n == 0 || v < 1 || v > 31 { return core::ptr::null_mut(); }
                    (*tm).tm_mday = v as i32;
                    si += n;
                }
                b'H' | b'k' => {
                    let (v, n) = parse_num(sbytes, si, 2);
                    if n == 0 || v > 23 { return core::ptr::null_mut(); }
                    (*tm).tm_hour = v as i32;
                    si += n;
                }
                b'M' => {
                    let (v, n) = parse_num(sbytes, si, 2);
                    if n == 0 || v > 59 { return core::ptr::null_mut(); }
                    (*tm).tm_min = v as i32;
                    si += n;
                }
                b'S' => {
                    let (v, n) = parse_num(sbytes, si, 2);
                    if n == 0 || v > 60 { return core::ptr::null_mut(); }
                    (*tm).tm_sec = v as i32;
                    si += n;
                }
                b'j' => {
                    let (v, n) = parse_num(sbytes, si, 3);
                    if n == 0 || v < 1 || v > 366 { return core::ptr::null_mut(); }
                    (*tm).tm_yday = (v as i32) - 1;
                    si += n;
                }
                b'a' | b'A' => {
                    // Weekday name: consume up to 3 chars if alphabetic.
                    let start = si;
                    while si < sbytes.len() && sbytes[si].is_ascii_alphabetic() { si += 1; }
                    if si == start { return core::ptr::null_mut(); }
                    // Not storing wday; can compute later.
                }
                b'b' | b'B' | b'h' => {
                    let start = si;
                    while si < sbytes.len() && sbytes[si].is_ascii_alphabetic() { si += 1; }
                    if si == start { return core::ptr::null_mut(); }
                    // Month name not directly stored beyond numeric; ignore.
                }
                b'n' | b't' => {
                    while si < sbytes.len() && sbytes[si].is_ascii_whitespace() { si += 1; }
                }
                b'%' => {
                    if si >= sbytes.len() || sbytes[si] != b'%' { return core::ptr::null_mut(); }
                    si += 1;
                }
                _ => return core::ptr::null_mut(),
            }
        }
        // Return pointer to next char in s after parsed portion.
        s.add(si) as *mut c_char
    }
}
fn parse_num(s: &[u8], off: usize, maxw: usize) -> (u32, usize) {
    let mut v = 0u32;
    let mut n = 0usize;
    while n < maxw && off + n < s.len() && s[off + n].is_ascii_digit() {
        v = v * 10 + (s[off + n] - b'0') as u32;
        n += 1;
    }
    (v, n)
}
#[unsafe(no_mangle)]
pub extern "C" fn tzset() {}
#[unsafe(no_mangle)]
pub extern "C" fn settimeofday(_tv: *const c_void, _tz: *const c_void) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}
