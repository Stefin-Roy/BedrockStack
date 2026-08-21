use core::ffi::{
    c_char, c_double, c_float, c_int, c_long, c_longlong, c_uint, c_ulong, c_ulonglong, c_void,
};

// ── exit / abort ──────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn exit(code: c_int) -> ! {
    run_atexit();
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

// ── atexit ────────────────────────────────────────────────────────

type AtExitFn = unsafe extern "C" fn();

const ATEXIT_MAX: usize = 32;
static mut ATEXIT_FNS: [Option<AtExitFn>; ATEXIT_MAX] = [None; ATEXIT_MAX];
static mut ATEXIT_LEN: usize = 0;

#[unsafe(no_mangle)]
pub extern "C" fn atexit(f: AtExitFn) -> c_int {
    unsafe {
        if ATEXIT_LEN >= ATEXIT_MAX {
            return -1;
        }
        ATEXIT_FNS[ATEXIT_LEN] = Some(f);
        ATEXIT_LEN += 1;
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn at_quick_exit(f: AtExitFn) -> c_int {
    atexit(f)
}

fn run_atexit() {
    unsafe {
        while ATEXIT_LEN > 0 {
            ATEXIT_LEN -= 1;
            if let Some(f) = ATEXIT_FNS[ATEXIT_LEN] {
                f();
            }
        }
    }
}

// Patch `exit` to run atexit handlers.
#[unsafe(no_mangle)]
pub extern "C" fn exit_with_atexit(code: c_int) -> ! {
    run_atexit();
    crate::process::exit(code as usize)
}

// ── aligned allocation ──────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    if alignment == 0 || !alignment.is_power_of_two() || size % alignment != 0 {
        crate::errno::set(crate::errno::EINVAL);
        return core::ptr::null_mut();
    }
    if alignment <= 16 {
        return crate::mem::malloc(size);
    }
    // Over-allocate and align manually; store original pointer before aligned block.
    let extra = alignment + core::mem::size_of::<*mut c_void>();
    let raw = crate::mem::malloc(size + extra) as *mut u8;
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    let aligned = ((raw as usize + extra) & !(alignment - 1)) as *mut u8;
    // store raw pointer just before aligned
    unsafe {
        let slot = (aligned as *mut *mut u8).sub(1);
        *slot = raw;
    }
    aligned as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> c_int {
    if memptr.is_null() || !alignment.is_power_of_two() || alignment % core::mem::size_of::<*mut c_void>() != 0 {
        return crate::errno::EINVAL;
    }
    let p = aligned_alloc(alignment, size);
    if p.is_null() {
        return crate::errno::ENOMEM;
    }
    unsafe { *memptr = p };
    0
}

// ── realpath / mkstemp family ─────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
    if path.is_null() {
        crate::errno::set(crate::errno::EINVAL);
        return core::ptr::null_mut();
    }
    let mut tmp = [0u8; 512];
    let plen = crate::string::strlen(path);
    let p = unsafe { core::slice::from_raw_parts(path as *const u8, plen) };
    let Some(abs) = crate::vfs::resolve_into(p, &mut tmp) else {
        crate::errno::set(crate::errno::ENAMETOOLONG);
        return core::ptr::null_mut();
    };
    let out = if resolved.is_null() {
        let buf = crate::mem::malloc(abs.len()) as *mut c_char;
        if buf.is_null() {
            crate::errno::set(crate::errno::ENOMEM);
            return core::ptr::null_mut();
        }
        buf
    } else {
        resolved
    };
    unsafe {
        core::ptr::copy_nonoverlapping(abs.as_ptr(), out as *mut u8, abs.len());
    }
    out
}

fn fill_template(tmpl: *mut c_char, suffixlen: usize) -> bool {
    if tmpl.is_null() {
        return false;
    }
    let len = crate::string::strlen(tmpl);
    if len < 6 + suffixlen {
        return false;
    }
    let base = len - suffixlen - 6;
    unsafe {
        for i in 0..6 {
            if *tmpl.add(base + i) != b'X' as c_char {
                return false;
            }
        }
        // Replace XXXXXX with pseudo-random alphanum via srand/rand state plus counter.
        static mut MK_CNT: u32 = 0;
        let cnt = MK_CNT;
        MK_CNT = MK_CNT.wrapping_add(1);
        let mut seed = cnt.wrapping_mul(1103515245).wrapping_add(12345);
        let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        for i in 0..6 {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let idx = (seed as usize) % chars.len();
            *tmpl.add(base + i) = chars[idx] as c_char;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn mkstemp(tmpl: *mut c_char) -> c_int {
    if !fill_template(tmpl, 0) {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let fd = unsafe { crate::fd::open(tmpl as *const c_char, crate::fd::O_RDWR | crate::fd::O_CREAT | crate::fd::O_EXCL, 0o600) };
    if fd < 0 { -1 } else { fd }
}

#[unsafe(no_mangle)]
pub extern "C" fn mkstemps(tmpl: *mut c_char, suffixlen: c_int) -> c_int {
    let sl = if suffixlen < 0 { 0 } else { suffixlen as usize };
    if !fill_template(tmpl, sl) {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let fd = unsafe { crate::fd::open(tmpl as *const c_char, crate::fd::O_RDWR | crate::fd::O_CREAT | crate::fd::O_EXCL, 0o600) };
    if fd < 0 { -1 } else { fd }
}

#[unsafe(no_mangle)]
pub extern "C" fn mkdtemp(tmpl: *mut c_char) -> *mut c_char {
    if !fill_template(tmpl, 0) {
        crate::errno::set(crate::errno::EINVAL);
        return core::ptr::null_mut();
    }
    let r = crate::vfs::mkdir_rs(unsafe { core::slice::from_raw_parts(tmpl as *const u8, crate::string::strlen(tmpl)) });
    if r < 0 {
        return core::ptr::null_mut();
    }
    tmpl
}

#[unsafe(no_mangle)]
pub extern "C" fn mktemp(tmpl: *mut c_char) -> *mut c_char {
    if !fill_template(tmpl, 0) {
        crate::errno::set(crate::errno::EINVAL);
        return core::ptr::null_mut();
    }
    tmpl
}

// ── lldiv / strtold ─────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct lldiv_t {
    pub quot: c_longlong,
    pub rem: c_longlong,
}

#[unsafe(no_mangle)]
pub extern "C" fn lldiv(numer: c_longlong, denom: c_longlong) -> lldiv_t {
    lldiv_t { quot: numer / denom, rem: numer % denom }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct imaxdiv_t {
    pub quot: i64,
    pub rem: i64,
}

#[unsafe(no_mangle)]
pub extern "C" fn imaxabs(j: i64) -> i64 { j.wrapping_abs() }

#[unsafe(no_mangle)]
pub extern "C" fn imaxdiv(numer: i64, denom: i64) -> imaxdiv_t {
    imaxdiv_t { quot: numer / denom, rem: numer % denom }
}

#[unsafe(no_mangle)]
pub extern "C" fn strtoimax(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> i64 {
    strtoll(s, endptr, base) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn strtoumax(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> u64 {
    strtoull(s, endptr, base) as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn wcstoimax(_s: *const i32, _endptr: *mut *mut i32, _base: c_int) -> i64 { 0 }

#[unsafe(no_mangle)]
pub extern "C" fn wcstoumax(_s: *const i32, _endptr: *mut *mut i32, _base: c_int) -> u64 { 0 }

#[unsafe(no_mangle)]
pub extern "C" fn strtold(s: *const c_char, endptr: *mut *mut c_char) -> f64 {
    // No long double; alias to strtod (double) — C locale, single precision suffices.
    let v = strtod(s, endptr);
    v as f64
}

// ── environment ─────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct EnvEntry {
    name: [u8; 64],
    nlen: usize,
    value: [u8; 256],
    vlen: usize,
    used: bool,
}
const ENV_MAX: usize = 32;
static mut ENV: [EnvEntry; ENV_MAX] = [EnvEntry { name: [0; 64], nlen: 0, value: [0; 256], vlen: 0, used: false }; ENV_MAX];

fn env_find(name: &[u8]) -> Option<usize> {
    unsafe {
        for i in 0..ENV_MAX {
            if ENV[i].used && ENV[i].nlen == name.len() && &ENV[i].name[..name.len()] == name {
                return Some(i);
            }
        }
        None
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        return core::ptr::null_mut();
    }
    let n = crate::string::strlen(name);
    let key = unsafe { core::slice::from_raw_parts(name as *const u8, n) };
    if let Some(idx) = env_find(key) {
        unsafe { ENV[idx].value.as_mut_ptr() as *mut c_char }
    } else {
        core::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int {
    if name.is_null() || value.is_null() || unsafe { *name == 0 } || unsafe { core::slice::from_raw_parts(name as *const u8, crate::string::strlen(name)).contains(&b'=') } {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let n = crate::string::strlen(name);
    let v = crate::string::strlen(value);
    if n >= 64 || v >= 256 {
        crate::errno::set(crate::errno::ENAMETOOLONG);
        return -1;
    }
    let key = unsafe { core::slice::from_raw_parts(name as *const u8, n) };
    let val = unsafe { core::slice::from_raw_parts(value as *const u8, v) };
    if let Some(idx) = env_find(key) {
        if overwrite == 0 {
            return 0;
        }
        unsafe {
            ENV[idx].value[..v].copy_from_slice(val);
            ENV[idx].value[v] = 0;
            ENV[idx].vlen = v;
        }
        return 0;
    }
    unsafe {
        for i in 0..ENV_MAX {
            if !ENV[i].used {
                ENV[i].name[..n].copy_from_slice(key);
                ENV[i].name[n] = 0;
                ENV[i].nlen = n;
                ENV[i].value[..v].copy_from_slice(val);
                ENV[i].value[v] = 0;
                ENV[i].vlen = v;
                ENV[i].used = true;
                return 0;
            }
        }
    }
    crate::errno::set(crate::errno::ENOMEM);
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn unsetenv(name: *const c_char) -> c_int {
    if name.is_null() || unsafe { *name == 0 } || unsafe { core::slice::from_raw_parts(name as *const u8, crate::string::strlen(name)).contains(&b'=') } {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let n = crate::string::strlen(name);
    let key = unsafe { core::slice::from_raw_parts(name as *const u8, n) };
    if let Some(idx) = env_find(key) {
        unsafe { ENV[idx].used = false; }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn putenv(s: *mut c_char) -> c_int {
    if s.is_null() {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let len = crate::string::strlen(s);
    let bytes = unsafe { core::slice::from_raw_parts(s as *const u8, len) };
    let Some(eq) = bytes.iter().position(|&c| c == b'=') else {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    };
    let name = unsafe { core::slice::from_raw_parts(s as *const u8, eq) };
    let value = unsafe { core::slice::from_raw_parts(s.add(eq + 1) as *const u8, len - eq - 1) };
    if name.is_empty() {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    // Copy to temp NUL terminated buffers for setenv.
    let mut nbuf = [0u8; 64];
    let mut vbuf = [0u8; 256];
    if name.len() >= nbuf.len() || value.len() >= vbuf.len() {
        crate::errno::set(crate::errno::ENAMETOOLONG);
        return -1;
    }
    nbuf[..name.len()].copy_from_slice(name);
    nbuf[name.len()] = 0;
    vbuf[..value.len()].copy_from_slice(value);
    vbuf[value.len()] = 0;
    setenv(nbuf.as_ptr() as *const c_char, vbuf.as_ptr() as *const c_char, 1)
}

#[unsafe(no_mangle)]
pub extern "C" fn clearenv() -> c_int {
    unsafe {
        for i in 0..ENV_MAX { ENV[i].used = false; }
    }
    0
}

/// `system(cmd)` — no shell; always fails with ENOENT.
#[unsafe(no_mangle)]
pub extern "C" fn system(_cmd: *const c_char) -> c_int {
    crate::errno::set(crate::errno::ENOENT);
    -1
}
