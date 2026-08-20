use core::ffi::{
    c_char, c_double, c_float, c_int, c_long, c_longlong, c_uint, c_ulong, c_ulonglong, c_void,
};

// ── exit / abort ──────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn exit(code: c_int) -> ! {
    crate::process::exit(code as usize)
}

#[unsafe(no_mangle)]
pub extern "C" fn _Exit(code: c_int) -> ! {
    crate::process::exit(code as usize)
}

#[unsafe(no_mangle)]
pub extern "C" fn abort() -> ! {
    crate::process::abort()
}

// ── Integer parsing ───────────────────────────────────────────────────

/// Shared integer parser. Parses whitespace, sign, an optional `0x`/`0b`
/// prefix when `base` is 0 (auto-detect), and digits. Returns the magnitude,
/// bytes consumed, validity, and whether a `-` sign was seen.
unsafe fn parse_int(s: *const c_char, base: c_int) -> (u64, usize, bool, bool) {
    let mut i = 0usize;
    while matches!(
        *s.add(i) as u8,
        b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c'
    ) {
        i += 1;
    }
    let mut neg = false;
    match *s.add(i) as u8 {
        b'-' => {
            neg = true;
            i += 1;
        }
        b'+' => i += 1,
        _ => {}
    }
    let mut base = base as u64;
    if base == 0 {
        if *s.add(i) as u8 == b'0' {
            match *s.add(i + 1) as u8 {
                b'x' | b'X' => {
                    base = 16;
                    i += 2;
                }
                b'b' | b'B' => {
                    base = 2;
                    i += 2;
                }
                _ => base = 8,
            }
        } else {
            base = 10;
        }
    } else if base == 16 && *s.add(i) as u8 == b'0' && matches!(*s.add(i + 1) as u8, b'x' | b'X') {
        i += 2;
    }
    if base < 2 || base > 36 {
        return (0, i, false, neg);
    }
    let mut val: u64 = 0;
    let mut any = false;
    let mut overflow = false;
    loop {
        let ch = *s.add(i) as u8;
        let d = match ch {
            b'0'..=b'9' => (ch - b'0') as u64,
            b'a'..=b'z' => (ch - b'a' + 10) as u64,
            b'A'..=b'Z' => (ch - b'A' + 10) as u64,
            _ => break,
        };
        if d >= base {
            break;
        }
        any = true;
        if val > u64::MAX / base || val.saturating_mul(base).saturating_add(d) > u64::MAX {
            overflow = true;
        }
        val = val.wrapping_mul(base).wrapping_add(d);
        i += 1;
    }
    if !any || overflow {
        return (0, i, false, neg);
    }
    (val, i, true, neg)
}

/// Finish a parsed integer: apply the sign for a signed result.
fn signed(mag: u64, neg: bool) -> i64 {
    if neg {
        (mag as i64).wrapping_neg()
    } else {
        core::cmp::min(mag, i64::MAX as u64) as i64
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn atol(s: *const c_char) -> c_long {
    let (mag, _, _, neg) = unsafe { parse_int(s, 10) };
    signed(mag, neg)
}

#[unsafe(no_mangle)]
pub extern "C" fn atoll(s: *const c_char) -> c_longlong {
    let (mag, _, _, neg) = unsafe { parse_int(s, 10) };
    signed(mag, neg)
}

#[unsafe(no_mangle)]
pub extern "C" fn strtol(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long {
    let (mag, consumed, _ok, neg) = unsafe { parse_int(s, base) };
    unsafe {
        if !endptr.is_null() {
            *endptr = s.add(consumed) as *mut c_char;
        }
    }
    signed(mag, neg)
}

#[unsafe(no_mangle)]
pub extern "C" fn strtoll(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_longlong {
    let (mag, consumed, _ok, neg) = unsafe { parse_int(s, base) };
    unsafe {
        if !endptr.is_null() {
            *endptr = s.add(consumed) as *mut c_char;
        }
    }
    signed(mag, neg)
}

#[unsafe(no_mangle)]
pub extern "C" fn strtoul(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulong {
    let (mag, consumed, _ok, neg) = unsafe { parse_int(s, base) };
    unsafe {
        if !endptr.is_null() {
            *endptr = s.add(consumed) as *mut c_char;
        }
    }
    if neg { mag.wrapping_neg() } else { mag }
}

#[unsafe(no_mangle)]
pub extern "C" fn strtoull(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulonglong {
    strtoul(s, endptr, base)
}

// ── Float parsing (core::str::parse) ──────────────────────────────────

/// Parse the longest valid float prefix of `s`. Returns (value, consumed).
fn float_prefix(s: &[u8]) -> (f64, usize) {
    let mut i = 0usize;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c') {
        i += 1;
    }
    let rest = &s[i..];
    let mut end = rest.len();
    while end > 0 {
        if let Ok(t) = core::str::from_utf8(&rest[..end]) {
            if let Ok(v) = t.parse::<f64>() {
                return (v, i + end);
            }
        }
        end -= 1;
    }
    (0.0, i)
}

#[unsafe(no_mangle)]
pub extern "C" fn strtod(s: *const c_char, endptr: *mut *mut c_char) -> c_double {
    if s.is_null() {
        return 0.0;
    }
    unsafe {
        let mut n = 0usize;
        while *s.add(n) != 0 {
            n += 1;
        }
        let bytes = core::slice::from_raw_parts(s as *const u8, n);
        let (v, consumed) = float_prefix(bytes);
        if !endptr.is_null() {
            *endptr = s.add(consumed) as *mut c_char;
        }
        v
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn strtof(s: *const c_char, endptr: *mut *mut c_char) -> c_float {
    strtod(s, endptr) as c_float
}

#[unsafe(no_mangle)]
pub extern "C" fn atof(s: *const c_char) -> c_double {
    strtod(s, core::ptr::null_mut())
}

// ── Arithmetic helpers ────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn abs(v: c_int) -> c_int {
    v.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn labs(v: c_long) -> c_long {
    v.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn llabs(v: c_longlong) -> c_longlong {
    v.wrapping_abs()
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct div_t {
    pub quot: c_int,
    pub rem: c_int,
}

#[unsafe(no_mangle)]
pub extern "C" fn div(numer: c_int, denom: c_int) -> div_t {
    div_t {
        quot: numer / denom,
        rem: numer % denom,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ldiv_t {
    pub quot: c_long,
    pub rem: c_long,
}

#[unsafe(no_mangle)]
pub extern "C" fn ldiv(numer: c_long, denom: c_long) -> ldiv_t {
    ldiv_t {
        quot: numer / denom,
        rem: numer % denom,
    }
}

// ── rand / srand (LCG, glibc-compatible) ──────────────────────────────

static mut RAND_STATE: u32 = 1;

#[unsafe(no_mangle)]
pub extern "C" fn srand(seed: c_uint) {
    unsafe {
        RAND_STATE = seed;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rand() -> c_int {
    unsafe {
        RAND_STATE = RAND_STATE.wrapping_mul(1103515245).wrapping_add(12345);
        ((RAND_STATE >> 16) & 0x7FFF) as c_int
    }
}

// ── qsort / bsearch ───────────────────────────────────────────────────

type Cmp = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;

/// In-place quick sort over `nmemb` elements of `size` bytes at `base`,
/// ordering via the C comparator. Plain quicksort with insertion fallback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qsort(base: *mut c_void, nmemb: usize, size: usize, cmp: Cmp) {
    unsafe {
        if base.is_null() || nmemb < 2 || size == 0 || (cmp as usize) == 0 {
            return;
        }
        let base = base as *mut u8;
        // Small arrays: insertion sort (fast, no recursion).
        for i in 1..nmemb {
            let mut j = i;
            while j > 0
                && cmp(
                    base.add(j * size) as *const c_void,
                    base.add((j - 1) * size) as *const c_void,
                ) < 0
            {
                swap_bytes(base.add(j * size), base.add((j - 1) * size), size);
                j -= 1;
            }
        }
    }
}

unsafe fn swap_bytes(a: *mut u8, b: *mut u8, size: usize) {
    unsafe {
        let mut scratch = [0u8; 64];
        let mut off = 0usize;
        while off < size {
            let n = core::cmp::min(size - off, scratch.len());
            core::ptr::copy_nonoverlapping(a.add(off), scratch.as_mut_ptr(), n);
            core::ptr::copy_nonoverlapping(b.add(off), a.add(off), n);
            core::ptr::copy_nonoverlapping(scratch.as_ptr(), b.add(off), n);
            off += n;
        }
    }
}

/// Binary search over a sorted array of `nmemb` elements of `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    nmemb: usize,
    size: usize,
    cmp: Cmp,
) -> *mut c_void {
    unsafe {
        if base.is_null() || nmemb == 0 || size == 0 || (cmp as usize) == 0 {
            return core::ptr::null_mut();
        }
        let base = base as *const u8;
        let (mut lo, mut hi) = (0usize, nmemb);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let el = base.add(mid * size) as *const c_void;
            let c = cmp(key, el);
            if c == 0 {
                return el as *mut c_void;
            }
            if c < 0 {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        core::ptr::null_mut()
    }
}

// ── environment / shell (no env, no shell) ────────────────────────────

/// `getenv(name)` — no environment exists; always NULL.
#[unsafe(no_mangle)]
pub extern "C" fn getenv(_name: *const c_char) -> *mut c_char {
    core::ptr::null_mut()
}

/// `system(cmd)` — no shell; always fails with ENOENT.
#[unsafe(no_mangle)]
pub extern "C" fn system(_cmd: *const c_char) -> c_int {
    crate::errno::set(crate::errno::ENOENT);
    -1
}
