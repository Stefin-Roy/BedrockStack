//! printf-family formatting engine — implemented entirely in permissive Rust.
//!
//! Supports `%c %s %p %% %d %i %u %o %x %X %f %F %e %E %g %G %n` with the
//! `- + space # 0` flags, `*`/digits width and precision, and the `hh h l ll
//! z t j L` length modifiers.  Floats are rendered by `core::fmt` (flt2dec —
//! exact round-trip shortest with precision), which this target enables (no
//! `no_fp_fmt_parse` feature).  `%g` approximates C's shortest-significant
//! form via `{}`; exponent fields are normalized to the C `e+00` style.

use core::ffi::{
    VaList, c_char, c_double, c_int, c_long, c_longlong, c_short, c_uint, c_ulong, c_ulonglong,
    c_ushort, c_void,
};
use core::fmt::Write as _;

use crate::stdio::FILE;

// ── sinks ─────────────────────────────────────────────────────────────

pub trait Sink {
    fn write(&mut self, b: &[u8]);
}

/// Bounded buffer sink (sprintf/snprintf). Writes up to `cap`; the logical
/// length keeps counting so `snprintf` returns the would-be length.
pub struct BufSink {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: usize,
}

impl Sink for BufSink {
    fn write(&mut self, b: &[u8]) {
        if self.len < self.cap {
            let n = core::cmp::min(b.len(), self.cap - self.len);
            unsafe {
                core::ptr::copy_nonoverlapping(b.as_ptr(), self.ptr.add(self.len), n);
            }
        }
        self.len += b.len();
    }
}

/// FILE* sink (fprintf/vfprintf) — routed through `fwrite`, which chunk-copies
/// through `.bss` scratch, so there are no per-byte syscalls.
pub struct FileSink {
    pub f: *mut FILE,
}

impl Sink for FileSink {
    fn write(&mut self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        if !self.f.is_null() {
            crate::stdio::fwrite(b.as_ptr() as *const c_void, 1, b.len(), self.f);
        }
    }
}

// ── float scratch ─────────────────────────────────────────────────────

static mut FLOAT_SCRATCH: [u8; 1024] = [0; 1024];

struct FloatBuf<'a> {
    buf: &'a mut [u8],
    len: &'a mut usize,
}

impl core::fmt::Write for FloatBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = core::cmp::min(s.len(), self.buf.len() - *self.len);
        self.buf[*self.len..*self.len + n].copy_from_slice(s.as_bytes());
        *self.len += n;
        Ok(())
    }
}

/// Normalize a core-fmt exponent to C style: `e3` → `e+03`, `e-3` → `e-03`,
/// `e21` → `e+21`. Returns the new length.
fn norm_exponent(buf: &mut [u8], len: usize, upper: bool) -> usize {
    let mut pos = None;
    for i in 0..len {
        if buf[i] == b'e' || buf[i] == b'E' {
            pos = Some(i);
            break;
        }
    }
    let Some(pos) = pos else {
        return len;
    };
    let mut j = pos + 1;
    let mut neg = false;
    if j < len && (buf[j] == b'-' || buf[j] == b'+') {
        neg = buf[j] == b'-';
        j += 1;
    }
    let mut digs = [0u8; 4];
    let mut di = 0usize;
    while j < len && di < digs.len() && buf[j].is_ascii_digit() {
        digs[di] = buf[j];
        di += 1;
        j += 1;
    }
    let mut suffix = [0u8; 6];
    let mut oi = 0usize;
    suffix[oi] = if upper { b'E' } else { b'e' };
    oi += 1;
    suffix[oi] = if neg { b'-' } else { b'+' };
    oi += 1;
    if di < 2 {
        suffix[oi] = b'0';
        oi += 1;
    }
    for k in 0..di {
        suffix[oi] = digs[k];
        oi += 1;
    }
    buf[pos..pos + oi].copy_from_slice(&suffix[..oi]);
    pos + oi
}

// ── shared emission helpers ───────────────────────────────────────────

/// Render the decimal/hex digits of `v` (base 10 or 16) in forward order.
fn radix_digits(mut v: u64, base: u8, upper: bool, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 24];
    let mut n = 0usize;
    if v == 0 {
        tmp[n] = b'0';
        n = 1;
    }
    while v > 0 {
        let d = (v % base as u64) as u8;
        tmp[n] = if d < 10 {
            b'0' + d
        } else {
            (if upper { b'A' } else { b'a' }) + (d - 10)
        };
        n += 1;
        v /= base as u64;
    }
    for i in 0..n {
        out[i] = tmp[n - 1 - i];
    }
    n
}

/// Emit `sign` + `prefix` + `digits[..ndigits]` applying width padding.
/// `zero_pad_ok` is false for integers with a precision (C ignores `0` then).
fn emit_field(
    sink: &mut dyn Sink,
    sign: u8,
    prefix: &[u8],
    digits: &[u8],
    ndigits: usize,
    width: usize,
    left: bool,
    zero: bool,
    zero_pad_ok: bool,
) {
    let sign_len = (sign != 0) as usize;
    let total = sign_len + prefix.len() + ndigits;
    let pad = width.saturating_sub(total);
    if left {
        if sign != 0 {
            sink.write(&[sign]);
        }
        if !prefix.is_empty() {
            sink.write(prefix);
        }
        sink.write(&digits[..ndigits]);
        for _ in 0..pad {
            sink.write(b" ");
        }
    } else if zero && zero_pad_ok {
        if sign != 0 {
            sink.write(&[sign]);
        }
        if !prefix.is_empty() {
            sink.write(prefix);
        }
        for _ in 0..pad {
            sink.write(b"0");
        }
        sink.write(&digits[..ndigits]);
    } else {
        for _ in 0..pad {
            sink.write(b" ");
        }
        if sign != 0 {
            sink.write(&[sign]);
        }
        if !prefix.is_empty() {
            sink.write(prefix);
        }
        sink.write(&digits[..ndigits]);
    }
}

// ── float emission ────────────────────────────────────────────────────

/// `mode`: b'f' fixed, b'e' scientific, b'g' shortest.
fn emit_float(
    sink: &mut dyn Sink,
    v: f64,
    prec: usize,
    width: usize,
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    upper: bool,
    mode: u8,
) {
    let scratch = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(FLOAT_SCRATCH) as *mut u8, 1024)
    };
    let mut len = 0usize;
    let mut w = FloatBuf {
        buf: scratch,
        len: &mut len,
    };

    let special = if v.is_nan() {
        Some(if upper { b"NAN" } else { b"nan" })
    } else if v.is_infinite() {
        Some(if upper { b"INF" } else { b"inf" })
    } else {
        None
    };

    let neg = v.is_sign_negative();
    let sign = if neg {
        b'-'
    } else if plus {
        b'+'
    } else if space {
        b' '
    } else {
        0
    };

    match special {
        Some(s) => {
            let _ = core::write!(w, "{}", core::str::from_utf8(s).unwrap_or("nan"));
        }
        None => {
            let av = v.abs();
            match mode {
                b'f' => {
                    let _ = core::write!(w, "{:.p$}", av, p = prec);
                }
                b'e' => {
                    let _ = core::write!(w, "{:.p$e}", av, p = prec);
                    len = norm_exponent(scratch, len, upper);
                }
                _ => {
                    let _ = core::write!(w, "{}", av);
                    len = norm_exponent(scratch, len, upper);
                }
            }
        }
    }

    let sign_len = (sign != 0) as usize;
    let total = sign_len + len;
    let pad = width.saturating_sub(total);
    if left {
        if sign != 0 {
            sink.write(&[sign]);
        }
        sink.write(&scratch[..len]);
        for _ in 0..pad {
            sink.write(b" ");
        }
    } else if zero {
        if sign != 0 {
            sink.write(&[sign]);
        }
        for _ in 0..pad {
            sink.write(b"0");
        }
        sink.write(&scratch[..len]);
    } else {
        for _ in 0..pad {
            sink.write(b" ");
        }
        if sign != 0 {
            sink.write(&[sign]);
        }
        sink.write(&scratch[..len]);
    }
}

// ── the engine ────────────────────────────────────────────────────────

/// Pull a signed magnitude by length modifier (returns sign + magnitude).
fn pull_signed(ap: &mut VaList, len: u8) -> (bool, u64) {
    match len {
        1 => {
            // hh — char promoted to int
            let v = unsafe { ap.next_arg::<c_int>() as i8 };
            (v < 0, (v as i64).unsigned_abs())
        }
        2 => {
            let v = unsafe { ap.next_arg::<c_int>() as i16 };
            (v < 0, (v as i64).unsigned_abs())
        }
        3 => {
            let v = unsafe { ap.next_arg::<c_long>() };
            (v < 0, (v as i64).unsigned_abs())
        }
        4 => {
            let v = unsafe { ap.next_arg::<c_longlong>() };
            (v < 0, (v as i64).unsigned_abs())
        }
        5 | 6 => {
            let v = unsafe { ap.next_arg::<isize>() };
            (v < 0, (v as i64).unsigned_abs())
        }
        7 => {
            let v = unsafe { ap.next_arg::<c_longlong>() };
            (v < 0, (v as i64).unsigned_abs())
        }
        _ => {
            let v = unsafe { ap.next_arg::<c_int>() };
            (v < 0, (v as i64).unsigned_abs())
        }
    }
}

/// Pull an unsigned value by length modifier.
fn pull_unsigned(ap: &mut VaList, len: u8) -> u64 {
    match len {
        1 => unsafe { ap.next_arg::<c_uint>() as u8 as u64 },
        2 => unsafe { ap.next_arg::<c_uint>() as u16 as u64 },
        3 => unsafe { ap.next_arg::<c_ulong>() as u64 },
        4 => unsafe { ap.next_arg::<c_ulonglong>() as u64 },
        5 | 6 => unsafe { ap.next_arg::<usize>() as u64 },
        7 => unsafe { ap.next_arg::<c_ulonglong>() as u64 },
        _ => unsafe { ap.next_arg::<c_uint>() as u64 },
    }
}

fn render(sink: &mut dyn Sink, fmt: &[u8], ap: &mut VaList) -> c_int {
    let mut i = 0usize;
    let mut count = 0usize;
    let mut digits = [0u8; 32];

    while i < fmt.len() {
        let c = fmt[i];
        if c != b'%' {
            sink.write(&[c]);
            count += 1;
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt.len() {
            break;
        }

        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        let mut space = false;
        let mut alt = false;
        loop {
            match fmt[i] {
                b'-' => {
                    left = true;
                    i += 1;
                }
                b'0' => {
                    zero = true;
                    i += 1;
                }
                b'+' => {
                    plus = true;
                    i += 1;
                }
                b' ' => {
                    space = true;
                    i += 1;
                }
                b'#' => {
                    alt = true;
                    i += 1;
                }
                _ => break,
            }
        }

        let mut width = 0usize;
        if i < fmt.len() && fmt[i] == b'*' {
            i += 1;
            let w = unsafe { ap.next_arg::<c_int>() };
            if w < 0 {
                left = true;
                width = w.unsigned_abs() as usize;
            } else {
                width = w as usize;
            }
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                width = width
                    .saturating_mul(10)
                    .saturating_add((fmt[i] - b'0') as usize);
                i += 1;
            }
        }

        let mut prec: isize = -1;
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            if i < fmt.len() && fmt[i] == b'*' {
                i += 1;
                let p = unsafe { ap.next_arg::<c_int>() };
                if p >= 0 {
                    prec = p as isize;
                }
            } else {
                prec = 0;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    prec = prec
                        .saturating_mul(10)
                        .saturating_add((fmt[i] - b'0') as isize);
                    i += 1;
                }
            }
        }

        let mut len = 0u8;
        if i < fmt.len() {
            match fmt[i] {
                b'h' => {
                    if i + 1 < fmt.len() && fmt[i + 1] == b'h' {
                        len = 1;
                        i += 2;
                    } else {
                        len = 2;
                        i += 1;
                    }
                }
                b'l' => {
                    if i + 1 < fmt.len() && fmt[i + 1] == b'l' {
                        len = 4;
                        i += 2;
                    } else {
                        len = 3;
                        i += 1;
                    }
                }
                b'z' => {
                    len = 5;
                    i += 1;
                }
                b't' => {
                    len = 6;
                    i += 1;
                }
                b'j' => {
                    len = 7;
                    i += 1;
                }
                b'L' => {
                    len = 8;
                    i += 1;
                }
                _ => {}
            }
        }

        let spec = if i < fmt.len() { fmt[i] } else { 0 };
        i += 1;

        match spec {
            b'%' => {
                sink.write(b"%");
                count += 1;
            }
            b'c' => {
                let v = unsafe { ap.next_arg::<c_int>() };
                let b = [(v & 0xFF) as u8];
                let pad = width.saturating_sub(1);
                if left {
                    sink.write(&b);
                    for _ in 0..pad {
                        sink.write(b" ");
                    }
                } else {
                    for _ in 0..pad {
                        sink.write(b" ");
                    }
                    sink.write(&b);
                }
                count += 1;
            }
            b's' => {
                let p = unsafe { ap.next_arg::<*const c_char>() };
                let mut slen = 0usize;
                if !p.is_null() {
                    while unsafe { *p.add(slen) } != 0 {
                        slen += 1;
                    }
                }
                if prec >= 0 && slen > prec as usize {
                    slen = prec as usize;
                }
                let pad = width.saturating_sub(slen);
                if left {
                    for j in 0..slen {
                        sink.write(&[unsafe { *p.add(j) } as u8]);
                    }
                    for _ in 0..pad {
                        sink.write(b" ");
                    }
                } else {
                    for _ in 0..pad {
                        sink.write(b" ");
                    }
                    for j in 0..slen {
                        sink.write(&[unsafe { *p.add(j) } as u8]);
                    }
                }
                count += slen;
            }
            b'd' | b'i' => {
                let (neg, mag) = pull_signed(ap, len);
                let sign = if neg {
                    b'-'
                } else if plus {
                    b'+'
                } else if space {
                    b' '
                } else {
                    0
                };
                let mut nd = radix_digits(mag, 10, false, &mut digits);
                let prec_applied = prec >= 0;
                if prec_applied {
                    let need = prec as usize;
                    if mag == 0 && need == 0 {
                        nd = 0;
                    } else if need > nd {
                        let zeros = need - nd;
                        unsafe {
                            core::ptr::copy(
                                digits.as_mut_ptr(),
                                digits.as_mut_ptr().add(zeros),
                                nd,
                            );
                        }
                        for k in 0..zeros {
                            digits[k] = b'0';
                        }
                        nd = need;
                    }
                }
                emit_field(
                    sink,
                    sign,
                    &[],
                    &digits,
                    nd,
                    width,
                    left,
                    zero,
                    !prec_applied,
                );
                count += nd + (sign != 0) as usize;
            }
            b'u' => {
                let mag = pull_unsigned(ap, len);
                let mut nd = radix_digits(mag, 10, false, &mut digits);
                let prec_applied = prec >= 0;
                if prec_applied {
                    let need = prec as usize;
                    if mag == 0 && need == 0 {
                        nd = 0;
                    } else if need > nd {
                        let zeros = need - nd;
                        unsafe {
                            core::ptr::copy(
                                digits.as_mut_ptr(),
                                digits.as_mut_ptr().add(zeros),
                                nd,
                            );
                        }
                        for k in 0..zeros {
                            digits[k] = b'0';
                        }
                        nd = need;
                    }
                }
                emit_field(sink, 0, &[], &digits, nd, width, left, zero, !prec_applied);
                count += nd;
            }
            b'o' => {
                let mag = pull_unsigned(ap, len);
                let mut nd = radix_digits(mag, 8, false, &mut digits);
                let prec_applied = prec >= 0;
                if prec_applied {
                    let need = prec as usize;
                    if mag == 0 && need == 0 {
                        nd = 0;
                    } else if need > nd {
                        let zeros = need - nd;
                        unsafe {
                            core::ptr::copy(
                                digits.as_mut_ptr(),
                                digits.as_mut_ptr().add(zeros),
                                nd,
                            );
                        }
                        for k in 0..zeros {
                            digits[k] = b'0';
                        }
                        nd = need;
                    }
                }
                let prefix: &[u8] = if alt && mag != 0 && nd == 0 {
                    b"0"
                } else if alt && mag != 0 && digits[0] != b'0' {
                    b"0"
                } else {
                    &[]
                };
                emit_field(
                    sink,
                    0,
                    prefix,
                    &digits,
                    nd,
                    width,
                    left,
                    zero,
                    !prec_applied,
                );
                count += nd + prefix.len();
            }
            b'x' | b'X' => {
                let upper = spec == b'X';
                let mag = pull_unsigned(ap, len);
                let mut nd = radix_digits(mag, 16, upper, &mut digits);
                let prec_applied = prec >= 0;
                if prec_applied {
                    let need = prec as usize;
                    if mag == 0 && need == 0 {
                        nd = 0;
                    } else if need > nd {
                        let zeros = need - nd;
                        unsafe {
                            core::ptr::copy(
                                digits.as_mut_ptr(),
                                digits.as_mut_ptr().add(zeros),
                                nd,
                            );
                        }
                        for k in 0..zeros {
                            digits[k] = b'0';
                        }
                        nd = need;
                    }
                }
                let prefix: &[u8] = if alt && mag != 0 {
                    if upper { b"0X" } else { b"0x" }
                } else {
                    &[]
                };
                emit_field(
                    sink,
                    0,
                    prefix,
                    &digits,
                    nd,
                    width,
                    left,
                    zero,
                    !prec_applied,
                );
                count += nd + prefix.len();
            }
            b'f' | b'F' => {
                let v = unsafe { ap.next_arg::<c_double>() };
                let p = if prec >= 0 { prec as usize } else { 6 };
                emit_float(
                    sink,
                    v,
                    p,
                    width,
                    left,
                    zero,
                    plus,
                    space,
                    spec == b'F',
                    b'f',
                );
            }
            b'e' | b'E' => {
                let v = unsafe { ap.next_arg::<c_double>() };
                let p = if prec >= 0 { prec as usize } else { 6 };
                emit_float(
                    sink,
                    v,
                    p,
                    width,
                    left,
                    zero,
                    plus,
                    space,
                    spec == b'E',
                    b'e',
                );
            }
            b'g' | b'G' => {
                let v = unsafe { ap.next_arg::<c_double>() };
                let p = if prec >= 0 { (prec as usize).max(1) } else { 6 };
                emit_float(
                    sink,
                    v,
                    p,
                    width,
                    left,
                    zero,
                    plus,
                    space,
                    spec == b'G',
                    b'g',
                );
            }
            b'p' => {
                let p = unsafe { ap.next_arg::<*const c_void>() };
                sink.write(b"0x");
                let nd = radix_digits(p as usize as u64, 16, false, &mut digits);
                sink.write(&digits[..nd]);
                count += 2 + nd;
            }
            b'n' => {
                // Store the count so far into the int pointer by length.
                match len {
                    1 => {
                        let p = unsafe { ap.next_arg::<*mut c_char>() };
                        if !p.is_null() {
                            unsafe { *p = (count & 0xFF) as c_char };
                        }
                    }
                    2 => {
                        let p = unsafe { ap.next_arg::<*mut c_short>() };
                        if !p.is_null() {
                            unsafe { *p = (count & 0xFFFF) as c_short };
                        }
                    }
                    3 => {
                        let p = unsafe { ap.next_arg::<*mut c_long>() };
                        if !p.is_null() {
                            unsafe { *p = count as c_long };
                        }
                    }
                    4 => {
                        let p = unsafe { ap.next_arg::<*mut c_longlong>() };
                        if !p.is_null() {
                            unsafe { *p = count as c_longlong };
                        }
                    }
                    5 | 6 => {
                        let p = unsafe { ap.next_arg::<*mut isize>() };
                        if !p.is_null() {
                            unsafe { *p = count as isize };
                        }
                    }
                    7 => {
                        let p = unsafe { ap.next_arg::<*mut c_longlong>() };
                        if !p.is_null() {
                            unsafe { *p = count as c_longlong };
                        }
                    }
                    _ => {
                        let p = unsafe { ap.next_arg::<*mut c_int>() };
                        if !p.is_null() {
                            unsafe { *p = count as c_int };
                        }
                    }
                }
            }
            _ => {
                sink.write(b"%");
                sink.write(&[spec]);
                count += 2;
            }
        }
    }
    count as c_int
}

fn fmt_slice(fmt: *const c_char) -> &'static [u8] {
    if fmt.is_null() {
        return b"";
    }
    let mut n = 0usize;
    while unsafe { *fmt.add(n) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(fmt as *const u8, n) }
}

// ── public API ────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfprintf(f: *mut FILE, fmt: *const c_char, ap: VaList) -> c_int {
    let mut ap2 = ap;
    let mut sink = FileSink { f };
    render(&mut sink, fmt_slice(fmt), &mut ap2)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fprintf(f: *mut FILE, fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        vfprintf(f, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vprintf(fmt: *const c_char, ap: VaList) -> c_int {
    vfprintf(unsafe { crate::stdio::stdout }, fmt, ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn printf(fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        vprintf(fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsnprintf(
    s: *mut c_char,
    n: usize,
    fmt: *const c_char,
    ap: VaList,
) -> c_int {
    let mut ap2 = ap;
    let mut sink = BufSink {
        ptr: s as *mut u8,
        cap: n,
        len: 0,
    };
    let r = render(&mut sink, fmt_slice(fmt), &mut ap2);
    if n > 0 && !s.is_null() {
        unsafe {
            let last = core::cmp::min(sink.len, n - 1);
            *s.add(last) = 0;
        }
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn snprintf(
    s: *mut c_char,
    n: usize,
    fmt: *const c_char,
    args: ...
) -> c_int {
    unsafe {
        let mut ap = args;
        vsnprintf(s, n, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsprintf(s: *mut c_char, fmt: *const c_char, ap: VaList) -> c_int {
    let mut ap2 = ap;
    let mut sink = BufSink {
        ptr: s as *mut u8,
        cap: usize::MAX,
        len: 0,
    };
    let r = render(&mut sink, fmt_slice(fmt), &mut ap2);
    if !s.is_null() {
        unsafe {
            *s.add(sink.len) = 0;
        }
    }
    r
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sprintf(s: *mut c_char, fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        vsprintf(s, fmt, ap)
    }
}

// ── helpers for stdio's dprintf / asprintf family ─────────────────────

/// Heap sink for `asprintf` — grows via `malloc`/`realloc`.
pub struct HeapSink {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: usize,
}

impl Sink for HeapSink {
    fn write(&mut self, b: &[u8]) {
        let need = self.len + b.len();
        if need > self.cap {
            let newcap = need.next_power_of_two().max(128);
            let np = unsafe { crate::mem::realloc(self.ptr as *mut core::ffi::c_void, newcap) };
            if np.is_null() {
                return;
            }
            self.ptr = np as *mut u8;
            self.cap = newcap;
        }
        if self.len < self.cap {
            let n = core::cmp::min(b.len(), self.cap - self.len);
            unsafe {
                core::ptr::copy_nonoverlapping(b.as_ptr(), self.ptr.add(self.len), n);
            }
            self.len += b.len();
        } else {
            self.len += b.len();
        }
    }
}

/// Format `fmt+ap` into `sink` (single pass, no `va_copy` needed).
pub unsafe fn format_to_sink(sink: &mut dyn Sink, fmt: *const c_char, ap: VaList) -> c_int {
    let mut ap2 = ap;
    render(sink, fmt_slice(fmt), &mut ap2)
}
