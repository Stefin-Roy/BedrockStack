use core::ffi::{c_char, c_int, c_long, c_uint, c_void};
use core::intrinsics::va_arg;
use core::ptr;

use crate::errno;
use crate::syscall;
use crate::syscall::{read_path, write_path};

// These buffers live in `.bss`, not on the fixed 32 KiB user stack.  The libc
// surface is single-threaded today, so one read and one write scratch buffer
// are sufficient for large file transfers.
static mut FREAD_SCRATCH: [u8; crate::IO_CHUNK_BYTES] = [0; crate::IO_CHUNK_BYTES];
static mut FWRITE_SCRATCH: [u8; crate::IO_CHUNK_BYTES] = [0; crate::IO_CHUNK_BYTES];

// ── fd-based streams ──────────────────────────────────────────────────

/// POSIX `write(1|2, ...)`: `/proc/self/std/out|err`. The kernel consumes the
/// syscall buffer in place (zero-filling it), so `syscall::write_data` chunk-
/// copies through static scratch storage and the caller's buffer stays intact.
#[unsafe(no_mangle)]
pub extern "C" fn write(fd: c_int, buf: *const c_void, len: usize) -> isize {
    let path: &[u8] = match fd {
        1 => b"/proc/self/std/out\0",
        2 => b"/proc/self/std/err\0",
        _ => {
            errno::set(9); // EBADF
            return -1;
        }
    };
    if len > 0 && buf.is_null() {
        errno::set(14); // EFAULT
        return -1;
    }
    let data: &[u8] = if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(buf as *const u8, len) }
    };
    let r = syscall::write_data(path, data, 0);
    errno::ret(r)
}

/// POSIX `read(0, ...)`: `/proc/self/std/in` (drains whatever is buffered).
#[unsafe(no_mangle)]
pub extern "C" fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize {
    if fd != 0 {
        errno::set(9); // EBADF
        return -1;
    }
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        errno::set(14); // EFAULT
        return -1;
    }
    let b = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    let r = unsafe { read_path(b"/proc/self/std/in\0", b, 0) };
    errno::ret(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn putchar(c: c_int) -> c_int {
    let b = (c & 0xFF) as u8;
    let r = syscall::write_data(b"/proc/self/std/out\0", &[b], 0);
    if r < 0 {
        errno::set((-r) as c_int);
        -1
    } else {
        c
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn puts(s: *const c_char) -> c_int {
    if s.is_null() {
        errno::set(22); // EINVAL
        return -1;
    }
    let len = crate::string::strlen(s);
    let mut line = [0u8; 256];
    let n = core::cmp::min(len, line.len() - 1);
    if n > 0 {
        unsafe {
            ptr::copy_nonoverlapping(s as *const u8, line.as_mut_ptr(), n);
        }
    }
    line[n] = b'\n';
    let r = syscall::write_data(b"/proc/self/std/out\0", &line[..n + 1], 0);
    if r < 0 {
        errno::set((-r) as c_int);
        -1
    } else {
        0
    }
}

// ── printf ────────────────────────────────────────────────────────────

/// Bounded byte sink for formatting; a full buffer silently truncates.
struct Fmt {
    buf: [u8; 512],
    pos: usize,
}

impl Fmt {
    fn new() -> Self {
        Fmt {
            buf: [0; 512],
            pos: 0,
        }
    }
    fn push(&mut self, b: u8) {
        if self.pos < self.buf.len() {
            self.buf[self.pos] = b;
            self.pos += 1;
        }
    }
    fn push_slice(&mut self, s: &[u8]) {
        for &b in s {
            self.push(b);
        }
    }
    fn pad(&mut self, ch: u8, n: usize) {
        for _ in 0..n {
            self.push(ch);
        }
    }
}

/// Emit `v` in `base` (10 or 16) into `f`.
fn push_uint(f: &mut Fmt, mut v: u64, base: u8, upper: bool) {
    let mut digits = [0u8; 64];
    let mut n = 0usize;
    if v == 0 {
        digits[0] = b'0';
        n = 1;
    }
    while v > 0 {
        let d = (v % base as u64) as u8;
        digits[n] = if d < 10 {
            b'0' + d
        } else {
            (if upper { b'A' } else { b'a' }) + (d - 10)
        };
        n += 1;
        v /= base as u64;
    }
    while n > 0 {
        n -= 1;
        f.push(digits[n]);
    }
}

/// Number of decimal digits in `v` (>= 1).
fn digits_10(mut v: u64) -> usize {
    let mut n = 1usize;
    while v >= 10 {
        v /= 10;
        n += 1;
    }
    n
}

/// Number of hex digits in `v` (>= 1).
fn digits_16(mut v: u64) -> usize {
    let mut n = 1usize;
    while v >= 16 {
        v /= 16;
        n += 1;
    }
    n
}

/// Minimal `printf`: `%c %s %d %i %u %x %X %p %%`, with `0`/`-` flags and a
/// decimal minimum width. Output goes to fd 1 (`/proc/self/std/out`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn printf(fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let mut ap = args;
        let mut f = Fmt::new();
        let mut i = 0usize;
        loop {
            let c = *fmt.add(i);
            if c == 0 {
                break;
            }
            if c != b'%' as c_char {
                f.push(c as u8);
                i += 1;
                continue;
            }
            i += 1;
            let mut left = false;
            let mut zero = false;
            let mut width = 0usize;
            loop {
                let d = *fmt.add(i) as u8;
                if d == b'-' {
                    left = true;
                    i += 1;
                } else if d == b'0' {
                    zero = true;
                    i += 1;
                } else if d.is_ascii_digit() {
                    width = width.saturating_mul(10).saturating_add((d - b'0') as usize);
                    i += 1;
                } else {
                    break;
                }
            }
            let spec = *fmt.add(i) as u8;
            i += 1;
            match spec {
                b'%' => f.push(b'%'),
                b'c' => {
                    let v: c_int = va_arg(&mut ap);
                    f.push((v & 0xFF) as u8);
                }
                b's' => {
                    let p: *const c_char = va_arg(&mut ap);
                    let mut len = 0usize;
                    if !p.is_null() {
                        while *p.add(len) != 0 {
                            len += 1;
                        }
                    }
                    if !left && width > len {
                        f.pad(b' ', width - len);
                    }
                    for j in 0..len {
                        f.push(*p.add(j) as u8);
                    }
                    if left && width > len {
                        f.pad(b' ', width - len);
                    }
                }
                b'd' | b'i' => {
                    let v: c_int = va_arg(&mut ap);
                    let neg = v < 0;
                    let mag = (v as i64).unsigned_abs();
                    let nd = digits_10(mag);
                    let tot = nd + neg as usize;
                    if !zero && !left && width > tot {
                        f.pad(b' ', width - tot);
                    }
                    if neg {
                        f.push(b'-');
                    }
                    if zero && !left && width > tot {
                        f.pad(b'0', width - tot);
                    }
                    push_uint(&mut f, mag, 10, false);
                    if left && width > tot {
                        f.pad(b' ', width - tot);
                    }
                }
                b'u' => {
                    let v: c_uint = va_arg(&mut ap);
                    let mag = v as u64;
                    let nd = digits_10(mag);
                    if !zero && !left && width > nd {
                        f.pad(b' ', width - nd);
                    }
                    if zero && !left && width > nd {
                        f.pad(b'0', width - nd);
                    }
                    push_uint(&mut f, mag, 10, false);
                    if left && width > nd {
                        f.pad(b' ', width - nd);
                    }
                }
                b'x' | b'X' => {
                    let v: c_uint = va_arg(&mut ap);
                    let mag = v as u64;
                    let nd = digits_16(mag);
                    if !zero && !left && width > nd {
                        f.pad(b' ', width - nd);
                    }
                    if zero && !left && width > nd {
                        f.pad(b'0', width - nd);
                    }
                    push_uint(&mut f, mag, 16, spec == b'X');
                    if left && width > nd {
                        f.pad(b' ', width - nd);
                    }
                }
                b'p' => {
                    let p: *const c_char = va_arg(&mut ap);
                    f.push_slice(b"0x");
                    push_uint(&mut f, p as usize as u64, 16, false);
                }
                _ => {
                    f.push(b'%');
                    if spec != 0 {
                        f.push(spec);
                    }
                }
            }
        }
        let n = f.pos;
        if n > 0 {
            syscall::write_data(b"/proc/self/std/out\0", &f.buf[..n], 0);
        }
        n as c_int
    }
}

// ── FILE* over VFS paths ──────────────────────────────────────────────

/// A path-addressed file handle. `fread`/`fwrite` use the VFS file object's
/// `arg4` flags: read-at offset, append (bit 0) or positioned write (offset
/// in bits 8..63). No buffering; each call is one (chunked) syscall.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FILE {
    path: [u8; 128],
    plen: usize,
    offset: u64,
    readable: bool,
    writable: bool,
    append: bool,
    slot: usize,
}

const FILE_POOL: usize = 16;

const FILE_INIT: FILE = FILE {
    path: [0; 128],
    plen: 0,
    offset: 0,
    readable: false,
    writable: false,
    append: false,
    slot: 0,
};

static mut FILES: [FILE; FILE_POOL] = [FILE_INIT; FILE_POOL];
static mut FILE_USED: [bool; FILE_POOL] = [false; FILE_POOL];

/// Parse `mode`, returning `(readable, writable, append, truncate)`.
fn parse_mode(m: &[u8]) -> Option<(bool, bool, bool, bool)> {
    // Strip any 'b' (binary) flag: our streams are byte-oriented, so the flag
    // is a no-op. Handles "rb", "wb", "ab", "rb+", "r+b", etc.
    let mut base = [0u8; 4];
    let mut n = 0usize;
    for &c in m {
        if c != b'b' && n < base.len() {
            base[n] = c;
            n += 1;
        }
    }
    match &base[..n] {
        b"r" => Some((true, false, false, false)),
        b"w" => Some((false, true, false, true)),
        b"a" => Some((false, true, true, false)),
        b"r+" => Some((true, true, false, false)),
        b"w+" => Some((true, true, false, true)),
        b"a+" => Some((true, true, true, false)),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut FILE {
    if path.is_null() || mode.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        let mlen = crate::string::strlen(mode);
        let m = core::slice::from_raw_parts(mode as *const u8, mlen);
        let Some((readable, writable, append, truncate)) = parse_mode(m) else {
            errno::set(22); // EINVAL
            return ptr::null_mut();
        };
        let plen = crate::string::strlen(path);
        if plen == 0 || plen >= 128 {
            errno::set(22);
            return ptr::null_mut();
        }
        let mut slot = None;
        for i in 0..FILE_POOL {
            if !FILE_USED[i] {
                slot = Some(i);
                break;
            }
        }
        let Some(i) = slot else {
            errno::set(24); // EMFILE
            return ptr::null_mut();
        };
        FILE_USED[i] = true;
        let f = &mut FILES[i];
        ptr::copy_nonoverlapping(path as *const u8, f.path.as_mut_ptr(), plen);
        f.path[plen] = 0;
        f.plen = plen;
        f.offset = 0;
        f.readable = readable;
        f.writable = writable;
        f.append = append;
        f.slot = i;
        if truncate {
            // `/path:truncate {len:0}` — build the method path and issue the call.
            let mut tp = [0u8; 140];
            tp[..plen].copy_from_slice(&f.path[..plen]);
            tp[plen..plen + 10].copy_from_slice(b":truncate\0");
            let mut pay = [0u8; 8];
            write_path(&tp[..plen + 10], &mut pay, 8, 0);
        }
        f as *mut FILE
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fclose(f: *mut FILE) -> c_int {
    if f.is_null() {
        return -1;
    }
    unsafe {
        let idx = (*f).slot;
        if idx < FILE_POOL {
            FILE_USED[idx] = false;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn fread(buf: *mut c_void, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || buf.is_null() {
        return 0;
    }
    let want = size.saturating_mul(count);
    if want == 0 {
        return 0;
    }
    unsafe {
        let fref = &mut *f;
        if !fref.readable {
            errno::set(9); // EBADF
            return 0;
        }
        let path = fref.path;
        let plen = fref.plen;
        let data = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(FREAD_SCRATCH) as *mut u8,
            crate::IO_CHUNK_BYTES,
        );
        let mut got = 0usize;
        while got < want {
            let chunk = core::cmp::min(want - got, data.len());
            let r = read_path(&path[..plen + 1], &mut data[..chunk], fref.offset);
            if r < 0 {
                errno::set((-r) as c_int);
                break;
            }
            let n = r as usize;
            if n == 0 {
                break;
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (buf as *mut u8).add(got), n);
            got += n;
            fref.offset += n as u64;
            if n < chunk {
                break;
            }
        }
        got / size
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fwrite(buf: *const c_void, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || buf.is_null() {
        return 0;
    }
    let want = size.saturating_mul(count);
    if want == 0 {
        return 0;
    }
    unsafe {
        let fref = &mut *f;
        if !fref.writable {
            errno::set(9); // EBADF
            return 0;
        }
        let path = fref.path;
        let plen = fref.plen;
        let base = fref.offset;
        let data = core::slice::from_raw_parts(buf as *const u8, want);
        let scratch = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(FWRITE_SCRATCH) as *mut u8,
            crate::IO_CHUNK_BYTES,
        );
        let mut done = 0usize;
        while done < want {
            let n = core::cmp::min(want - done, scratch.len());
            scratch[..n].copy_from_slice(&data[done..done + n]);
            let flags = if fref.append {
                0x1
            } else {
                (base + done as u64) << 8
            };
            let r = write_path(&path[..plen + 1], scratch, n, flags);
            if r < 0 {
                errno::set((-r) as c_int);
                break;
            }
            done += n;
        }
        if !fref.append {
            fref.offset = base + done as u64;
        }
        done / size
    }
}

/// File size in bytes via the VFS file object's `stat` method. The response is
/// packed LE: `{ino:u64, size:u64, kind:u32, mtime:u64}` (28 bytes), so the size
/// sits at `buf[8..16]`. The method input is a blob so a nonzero write buffer
/// round-trips the output back to us.
fn file_size(f: &FILE) -> Option<u64> {
    let mut tp = [0u8; 140];
    tp[..f.plen].copy_from_slice(&f.path[..f.plen]);
    tp[f.plen..f.plen + 6].copy_from_slice(b":stat\0");
    let mut buf = [0u8; 32];
    let r = unsafe { write_path(&tp[..f.plen + 6], &mut buf, 32, 0) };
    if r < 16 {
        return None;
    }
    Some(u64::from_le_bytes(buf[8..16].try_into().ok()?))
}

#[unsafe(no_mangle)]
pub extern "C" fn fseek(f: *mut FILE, offset: c_long, whence: c_int) -> c_int {
    if f.is_null() {
        errno::set(22); // EINVAL
        return -1;
    }
    unsafe {
        let fref = &mut *f;
        match whence {
            0 => {
                fref.offset = offset.max(0) as u64;
                0
            }
            1 => {
                fref.offset = (fref.offset as i64 + offset).max(0) as u64;
                0
            }
            2 => match file_size(fref) {
                Some(base) => {
                    fref.offset = (base as i64 + offset).max(0) as u64;
                    0
                }
                None => {
                    errno::set(22); // EINVAL
                    -1
                }
            },
            _ => {
                errno::set(22); // EINVAL
                -1
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ftell(f: *mut FILE) -> c_long {
    if f.is_null() {
        return -1;
    }
    unsafe { (&*f).offset as c_long }
}

#[unsafe(no_mangle)]
pub extern "C" fn fflush(_f: *mut FILE) -> c_int {
    0
}
