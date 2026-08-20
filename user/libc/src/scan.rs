//! scanf-family scanning engine — permissive Rust.
//!
//! Supports `%d %i %u %o %x %X %c %s %f %e %g %n %%` with width, `*`
//! suppression, and the `hh h l ll z t j L` length modifiers.  `%f` assigns a
//! `float*` (C promotes nothing here); `%lf`/`%Lf` assign a `double*`.
//! Floats are parsed with `str::parse::<f64>()` (dec2flt), available on this
//! target.

use core::ffi::{c_char, c_double, c_float, c_int, c_long, c_longlong, c_short, c_uchar, c_uint, c_ulong, c_ulonglong, c_ushort, c_void, VaList};

use crate::stdio::FILE;

// ── input sources ─────────────────────────────────────────────────────

pub trait ScanSrc {
    /// Read one byte; None at EOF.
    fn getc(&mut self) -> Option<u8>;
    /// Push a just-read byte back (single-slot).
    fn ungetc(&mut self, b: u8);
    /// Consume leading whitespace.
    fn skip_ws(&mut self) {
        loop {
            match self.getc() {
                Some(b) if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' => {}
                Some(b) => {
                    self.ungetc(b);
                    return;
                }
                None => return,
            }
        }
    }
}

/// Fixed string source (sscanf/vsscanf).
pub struct SrcStr<'a> {
    s: &'a [u8],
    pos: usize,
    back: Option<u8>,
}

impl<'a> SrcStr<'a> {
    pub fn new(s: &'a [u8]) -> Self {
        SrcStr {
            s,
            pos: 0,
            back: None,
        }
    }
}

impl ScanSrc for SrcStr<'_> {
    fn getc(&mut self) -> Option<u8> {
        if let Some(b) = self.back.take() {
            return Some(b);
        }
        if self.pos < self.s.len() {
            let b = self.s[self.pos];
            self.pos += 1;
            Some(b)
        } else {
            None
        }
    }
    fn ungetc(&mut self, b: u8) {
        self.back = Some(b);
    }
}

/// FILE* source (fscanf/vfscanf), via `fgetc` + one slot of pushback.
pub struct SrcFile {
    f: *mut FILE,
    back: Option<u8>,
}

impl SrcFile {
    pub fn new(f: *mut FILE) -> Self {
        SrcFile { f, back: None }
    }
}

impl ScanSrc for SrcFile {
    fn getc(&mut self) -> Option<u8> {
        if let Some(b) = self.back.take() {
            return Some(b);
        }
        let c = crate::stdio::fgetc(self.f);
        if c < 0 {
            None
        } else {
            Some((c & 0xFF) as u8)
        }
    }
    fn ungetc(&mut self, b: u8) {
        self.back = Some(b);
    }
}

/// stdin source (scanf/vscanf), via `getchar` + one slot of pushback.
pub struct SrcStdin {
    back: Option<u8>,
}

impl SrcStdin {
    pub fn new() -> Self {
        SrcStdin { back: None }
    }
}

impl ScanSrc for SrcStdin {
    fn getc(&mut self) -> Option<u8> {
        if let Some(b) = self.back.take() {
            return Some(b);
        }
        let c = crate::stdio::getchar();
        if c < 0 {
            None
        } else {
            Some((c & 0xFF) as u8)
        }
    }
    fn ungetc(&mut self, b: u8) {
        self.back = Some(b);
    }
}

// ── assignments ───────────────────────────────────────────────────────

fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' || b == 0x0b || b == 0x0c
}

/// Consume up to `width` chars from `src`, storing them in `out` (which must
/// be big enough). Returns the number consumed (or None on immediate EOF).
fn read_token(src: &mut dyn ScanSrc, out: &mut [u8], width: usize) -> Option<usize> {
    let mut n = 0usize;
    while n < width {
        match src.getc() {
            Some(b) => {
                out[n] = b;
                n += 1;
            }
            None => break,
        }
    }
    Some(n)
}

/// Parse an integer token. `base`: 10 (d/u), 8 (o), 16 (x/X), or -1 for %i
/// autodetection. Returns `(magnitude, negative, ok)`.
fn scan_int(src: &mut dyn ScanSrc, base: i32, width: usize) -> Option<(u64, bool, bool)> {
    let mut tok = [0u8; 64];
    let mut n = 0usize;
    let mut neg = false;
    let mut started = false;

    // Sign.
    if n < width {
        match src.getc() {
            Some(b'+') => {
                n += 1;
            }
            Some(b'-') => {
                neg = true;
                n += 1;
            }
            Some(b) => {
                src.ungetc(b);
            }
            None => {
                if !started {
                    return None;
                }
            }
        }
    }
    started = n > 0;

    let mut effective_base = base as u32;
    if base < 0 {
        // %i: peek for 0x/0 prefix.
        if n < width {
            if let Some(b) = src.getc() {
                if b == b'0' {
                    tok[n] = b;
                    n += 1;
                    if n < width {
                        match src.getc() {
                            Some(x) if x == b'x' || x == b'X' => {
                                tok[n] = x;
                                n += 1;
                                effective_base = 16;
                            }
                            Some(x) => {
                                src.ungetc(x);
                                effective_base = 8;
                            }
                            None => {}
                        }
                    } else {
                        effective_base = 8;
                    }
                } else {
                    src.ungetc(b);
                    effective_base = 10;
                }
            }
        } else {
            effective_base = 10;
        }
    }

    let mut value: u64 = 0;
    let mut any = false;
    while n < width {
        let Some(b) = src.getc() else { break };
        let d = match b {
            b'0'..=b'9' => Some((b - b'0') as u64),
            b'a'..=b'f' if effective_base == 16 => Some((b - b'a' + 10) as u64),
            b'A'..=b'F' if effective_base == 16 => Some((b - b'A' + 10) as u64),
            _ => None,
        };
        match d {
            Some(d) if d < effective_base as u64 => {
                value = value.saturating_mul(effective_base as u64).saturating_add(d);
                tok[n] = b;
                n += 1;
                any = true;
            }
            _ => {
                src.ungetc(b);
                break;
            }
        }
    }

    // If we consumed a 0x/0 prefix but no digits after, that's a failure for
    // hex/octal ("0x" alone assigns nothing).
    if !any && effective_base != 10 {
        if neg || n > 0 {
            // consumed a sign or prefix; that's not a valid number — failure.
        }
        return Some((0, false, false));
    }
    if !any {
        return Some((0, false, false));
    }
    Some((value, neg, true))
}

/// Parse a float token and return the f64, or None on failure.
fn scan_float(src: &mut dyn ScanSrc, width: usize) -> Option<f64> {
    let mut tok = [0u8; 64];
    let mut n = 0usize;
    let mut phase = 0u8; // 0 start, 1 mantissa, 2 exponent
    while n < width {
        let Some(b) = src.getc() else { break };
        let ok = match b {
            b'0'..=b'9' => {
                if phase < 3 {
                    phase = 1;
                }
                true
            }
            b'+' | b'-' => phase == 0 || phase == 2,
            b'.' => phase == 0 || phase == 1,
            b'e' | b'E' => phase == 1,
            _ => false,
        };
        if ok {
            tok[n] = b;
            n += 1;
            if b == b'e' || b == b'E' {
                phase = 2;
            }
        } else {
            src.ungetc(b);
            break;
        }
    }
    if n == 0 {
        return None;
    }
    // A trailing sign after 'e' with no digits is invalid — trim it.
    let mut len = n;
    if len >= 2 {
        let last = tok[len - 1];
        if (last == b'+' || last == b'-') && tok[len - 2] == b'e' {
            // 'e' alone isn't valid either; trim the whole 'e' sign.
            len -= 2;
        } else if last == b'e' || last == b'E' {
            len -= 1;
        }
    }
    if len == 0 {
        return None;
    }
    core::str::from_utf8(&tok[..len]).ok()?.parse::<f64>().ok()
}

// ── the engine ────────────────────────────────────────────────────────

fn scan_engine(src: &mut dyn ScanSrc, fmt: &[u8], ap: &mut VaList) -> c_int {
    let mut fi = 0usize;
    let mut nassign = 0i32;
    let mut nchars = 0usize;

    while fi < fmt.len() {
        let c = fmt[fi];
        if c == b'%' {
            fi += 1;
            if fi >= fmt.len() {
                break;
            }
            let mut suppress = false;
            let mut width = 0usize;
            let mut width_set = false;
            loop {
                let d = fmt[fi];
                if d == b'*' {
                    suppress = true;
                    fi += 1;
                } else if d.is_ascii_digit() {
                    width = width.saturating_mul(10).saturating_add((d - b'0') as usize);
                    width_set = true;
                    fi += 1;
                } else {
                    break;
                }
            }
            let mut len = 0u8;
            if fi < fmt.len() {
                match fmt[fi] {
                    b'h' => {
                        if fi + 1 < fmt.len() && fmt[fi + 1] == b'h' {
                            len = 1;
                            fi += 2;
                        } else {
                            len = 2;
                            fi += 1;
                        }
                    }
                    b'l' => {
                        if fi + 1 < fmt.len() && fmt[fi + 1] == b'l' {
                            len = 4;
                            fi += 2;
                        } else {
                            len = 3;
                            fi += 1;
                        }
                    }
                    b'z' => {
                        len = 5;
                        fi += 1;
                    }
                    b't' => {
                        len = 6;
                        fi += 1;
                    }
                    b'j' => {
                        len = 7;
                        fi += 1;
                    }
                    b'L' => {
                        len = 8;
                        fi += 1;
                    }
                    _ => {}
                }
            }
            let spec = if fi < fmt.len() { fmt[fi] } else { 0 };
            fi += 1;

            if width_set && width == 0 {
                // "A field width of 0 is undefined; treat as no width."
                width = usize::MAX;
            } else if !width_set {
                width = usize::MAX;
            }

            match spec {
                b'%' => {
                    if src.getc() != Some(b'%') {
                        break;
                    }
                    nchars += 1;
                }
                b'c' => {
                    let w = if width_set { width } else { 1 };
                    if !suppress {
                        let dst = unsafe { ap.next_arg::<*mut c_char>() };
                        if dst.is_null() {
                            break;
                        }
                        let mut got = 0usize;
                        while got < w {
                            match src.getc() {
                                Some(b) => {
                                    unsafe {
                                        *dst.add(got) = b as c_char;
                                    }
                                    got += 1;
                                }
                                None => break,
                            }
                        }
                        nchars += got;
                        if got < w {
                            // input failure mid-assignment: stop.
                            break;
                        }
                        nassign += 1;
                    } else {
                        let mut got = 0usize;
                        while got < w {
                            if src.getc().is_none() {
                                break;
                            }
                            got += 1;
                        }
                        nchars += got;
                    }
                }
                b's' => {
                    src.skip_ws();
                    let mut buf = [0u8; 256];
                    let mut got = 0usize;
                    while got < width {
                        match src.getc() {
                            Some(b) if !is_space(b) => {
                                buf[got] = b;
                                got += 1;
                            }
                            Some(b) => {
                                src.ungetc(b);
                                break;
                            }
                            None => break,
                        }
                    }
                    nchars += got;
                    if !suppress {
                        let dst = unsafe { ap.next_arg::<*mut c_char>() };
                        if dst.is_null() {
                            break;
                        }
                        if got == 0 {
                            break; // no match — input failure
                        }
                        unsafe {
                            core::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, got);
                            *dst.add(got) = 0;
                        }
                        nassign += 1;
                    }
                }
                b'd' => {
                    if let Some((mag, neg, ok)) = scan_int(src, 10, width) {
                        if ok && !suppress {
                            assign_int(ap, len, neg, mag);
                            nassign += 1;
                        }
                    } else {
                        return -1;
                    }
                }
                b'i' => {
                    if let Some((mag, neg, ok)) = scan_int(src, -1, width) {
                        if ok && !suppress {
                            assign_int(ap, len, neg, mag);
                            nassign += 1;
                        }
                    } else {
                        return -1;
                    }
                }
                b'u' => {
                    if let Some((mag, _, ok)) = scan_int(src, 10, width) {
                        if ok && !suppress {
                            assign_uint(ap, len, mag);
                            nassign += 1;
                        }
                    } else {
                        return -1;
                    }
                }
                b'o' => {
                    if let Some((mag, _, ok)) = scan_int(src, 8, width) {
                        if ok && !suppress {
                            assign_uint(ap, len, mag);
                            nassign += 1;
                        }
                    } else {
                        return -1;
                    }
                }
                b'x' | b'X' => {
                    if let Some((mag, _, ok)) = scan_int(src, 16, width) {
                        if ok && !suppress {
                            assign_uint(ap, len, mag);
                            nassign += 1;
                        }
                    } else {
                        return -1;
                    }
                }
                b'f' | b'e' | b'g' => {
                    let r = scan_float(src, width);
                    match r {
                        Some(v) => {
                            if !suppress {
                                assign_float(ap, len, v);
                                nassign += 1;
                            }
                        }
                        None => {
                            if nassign == 0 {
                                return -1;
                            }
                        }
                    }
                }
                b'n' => {
                    if !suppress {
                        assign_n(ap, len, nchars);
                    }
                }
                _ => {
                    break;
                }
            }
        } else if is_space(c) {
            src.skip_ws();
            fi += 1;
        } else {
            match src.getc() {
                Some(b) if b == c => {
                    nchars += 1;
                    fi += 1;
                }
                Some(b) => {
                    src.ungetc(b);
                    break;
                }
                None => break,
            }
        }
    }
    nassign
}

/// Assign a signed integer per length modifier.
fn assign_int(ap: &mut VaList, len: u8, neg: bool, mag: u64) {
    let val: i64 = if neg {
        -(mag as i64)
    } else {
        mag as i64
    };
    match len {
        1 => {
            let p = unsafe { ap.next_arg::<*mut c_char>() };
            if !p.is_null() {
                unsafe { *p = (val as i8) as c_char };
            }
        }
        2 => {
            let p = unsafe { ap.next_arg::<*mut c_short>() };
            if !p.is_null() {
                unsafe { *p = val as c_short };
            }
        }
        3 => {
            let p = unsafe { ap.next_arg::<*mut c_long>() };
            if !p.is_null() {
                unsafe { *p = val as c_long };
            }
        }
        4 => {
            let p = unsafe { ap.next_arg::<*mut c_longlong>() };
            if !p.is_null() {
                unsafe { *p = val as c_longlong };
            }
        }
        5 => {
            let p = unsafe { ap.next_arg::<*mut usize>() };
            if !p.is_null() {
                unsafe { *p = mag as usize };
            }
        }
        6 => {
            let p = unsafe { ap.next_arg::<*mut isize>() };
            if !p.is_null() {
                unsafe { *p = val as isize };
            }
        }
        7 => {
            let p = unsafe { ap.next_arg::<*mut c_longlong>() };
            if !p.is_null() {
                unsafe { *p = val as c_longlong };
            }
        }
        _ => {
            let p = unsafe { ap.next_arg::<*mut c_int>() };
            if !p.is_null() {
                unsafe { *p = val as c_int };
            }
        }
    }
}

/// Assign an unsigned integer per length modifier.
fn assign_uint(ap: &mut VaList, len: u8, mag: u64) {
    match len {
        1 => {
            let p = unsafe { ap.next_arg::<*mut c_uchar>() };
            if !p.is_null() {
                unsafe { *p = mag as c_uchar };
            }
        }
        2 => {
            let p = unsafe { ap.next_arg::<*mut c_ushort>() };
            if !p.is_null() {
                unsafe { *p = mag as c_ushort };
            }
        }
        3 => {
            let p = unsafe { ap.next_arg::<*mut c_ulong>() };
            if !p.is_null() {
                unsafe { *p = mag as c_ulong };
            }
        }
        4 => {
            let p = unsafe { ap.next_arg::<*mut c_ulonglong>() };
            if !p.is_null() {
                unsafe { *p = mag as c_ulonglong };
            }
        }
        5 => {
            let p = unsafe { ap.next_arg::<*mut usize>() };
            if !p.is_null() {
                unsafe { *p = mag as usize };
            }
        }
        6 => {
            let p = unsafe { ap.next_arg::<*mut usize>() };
            if !p.is_null() {
                unsafe { *p = mag as usize };
            }
        }
        7 => {
            let p = unsafe { ap.next_arg::<*mut c_ulonglong>() };
            if !p.is_null() {
                unsafe { *p = mag as c_ulonglong };
            }
        }
        _ => {
            let p = unsafe { ap.next_arg::<*mut c_uint>() };
            if !p.is_null() {
                unsafe { *p = mag as c_uint };
            }
        }
    }
}

/// Assign a float per length modifier. C promotes nothing: `%f` → float*,
/// `%lf`/`%Lf` → double*.
fn assign_float(ap: &mut VaList, len: u8, v: f64) {
    if len == 3 || len == 8 {
        let p = unsafe { ap.next_arg::<*mut c_double>() };
        if !p.is_null() {
            unsafe { *p = v };
        }
    } else {
        let p = unsafe { ap.next_arg::<*mut c_float>() };
        if !p.is_null() {
            unsafe { *p = v as c_float };
        }
    }
}

/// Assign `%n` (count of chars consumed) per length modifier.
fn assign_n(ap: &mut VaList, len: u8, n: usize) {
    match len {
        1 => {
            let p = unsafe { ap.next_arg::<*mut c_char>() };
            if !p.is_null() {
                unsafe { *p = n as c_char };
            }
        }
        2 => {
            let p = unsafe { ap.next_arg::<*mut c_short>() };
            if !p.is_null() {
                unsafe { *p = n as c_short };
            }
        }
        3 => {
            let p = unsafe { ap.next_arg::<*mut c_long>() };
            if !p.is_null() {
                unsafe { *p = n as c_long };
            }
        }
        4 => {
            let p = unsafe { ap.next_arg::<*mut c_longlong>() };
            if !p.is_null() {
                unsafe { *p = n as c_longlong };
            }
        }
        5 | 6 => {
            let p = unsafe { ap.next_arg::<*mut usize>() };
            if !p.is_null() {
                unsafe { *p = n as usize };
            }
        }
        7 => {
            let p = unsafe { ap.next_arg::<*mut c_longlong>() };
            if !p.is_null() {
                unsafe { *p = n as c_longlong };
            }
        }
        _ => {
            let p = unsafe { ap.next_arg::<*mut c_int>() };
            if !p.is_null() {
                unsafe { *p = n as c_int };
            }
        }
    }
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

fn str_slice(s: *const c_char) -> &'static [u8] {
    if s.is_null() {
        return b"";
    }
    let mut n = 0usize;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(s as *const u8, n) }
}

// ── public API ────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsscanf(s: *const c_char, fmt: *const c_char, ap: VaList) -> c_int {
    let mut ap2 = ap;
    let mut src = SrcStr::new(str_slice(s));
    scan_engine(&mut src, fmt_slice(fmt), &mut ap2)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sscanf(s: *const c_char, fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        vsscanf(s, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfscanf(f: *mut FILE, fmt: *const c_char, ap: VaList) -> c_int {
    let mut ap2 = ap;
    let mut src = SrcFile::new(f);
    scan_engine(&mut src, fmt_slice(fmt), &mut ap2)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fscanf(f: *mut FILE, fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        vfscanf(f, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vscanf(fmt: *const c_char, ap: VaList) -> c_int {
    let mut ap2 = ap;
    let mut src = SrcStdin::new();
    scan_engine(&mut src, fmt_slice(fmt), &mut ap2)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scanf(fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        vscanf(fmt, ap)
    }
}