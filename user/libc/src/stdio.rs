use core::ffi::{c_char, c_int, c_long, c_void};
use core::ptr;

use crate::errno;
use crate::syscall;
use crate::syscall::{read_path, write_path};

// printf-family lives in `format`; re-export for the pre-existing Rust call
// path (`libc::stdio::printf`).
pub use crate::format::{
    fprintf, printf, snprintf, sprintf, vfprintf, vprintf, vsnprintf, vsprintf,
};

// These buffers live in `.bss`, not on the fixed 32 KiB user stack.  The libc
// surface is single-threaded today, so one read and one write scratch buffer
// are sufficient for large file transfers.
static mut FREAD_SCRATCH: [u8; crate::IO_CHUNK_BYTES] = [0; crate::IO_CHUNK_BYTES];
static mut FWRITE_SCRATCH: [u8; crate::IO_CHUNK_BYTES] = [0; crate::IO_CHUNK_BYTES];

// ── fd-based streams ──────────────────────────────────────────────────

/// POSIX `write(fd, ...)`: fds 1/2 route to `/proc/self/std/out|err`, fds 3+
/// to an opened path fd. The kernel consumes the syscall buffer in place, so
/// `fd::write_fd` chunk-copies through static scratch and the caller's buffer
/// stays intact.
#[unsafe(no_mangle)]
pub extern "C" fn write(fd: c_int, buf: *const c_void, len: usize) -> isize {
    crate::fd::write_fd(fd, buf, len)
}

/// POSIX `read(fd, ...)`: fd 0 drains `/proc/self/std/in`, fds 3+ read an
/// opened path fd.
#[unsafe(no_mangle)]
pub extern "C" fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize {
    crate::fd::read_fd(fd, buf, len)
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
    let len = unsafe { crate::string::strlen(s) };
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

// ── standard streams ──────────────────────────────────────────────────

/// The three standard streams are real FILE objects backed by
/// `/proc/self/std/{in,out,err}`. The exported `stdin`/`stdout`/`stderr`
/// globals are pointers to them, matching the C `extern FILE *stdin;` ABI.
/// They are wired up by `stdio_init()`, which `crt` calls before `entry_main`.
static mut STDIN_OBJ: FILE = FILE_INIT;
static mut STDOUT_OBJ: FILE = FILE_INIT;
static mut STDERR_OBJ: FILE = FILE_INIT;

#[unsafe(no_mangle)]
pub static mut stdin: *mut FILE = core::ptr::null_mut();
#[unsafe(no_mangle)]
pub static mut stdout: *mut FILE = core::ptr::null_mut();
#[unsafe(no_mangle)]
pub static mut stderr: *mut FILE = core::ptr::null_mut();

/// One-time wiring of the standard stream handles.
pub fn stdio_init() {
    unsafe {
        let init_std = |obj: *mut FILE, path: &[u8], readable: bool, writable: bool| {
            let f = &mut *obj;
            *f = FILE_INIT;
            f.path[..path.len()].copy_from_slice(path);
            f.plen = path.len() - 1;
            f.readable = readable;
            f.writable = writable;
            f.slot = 0xFFFF;
        };
        init_std(
            core::ptr::addr_of_mut!(STDOUT_OBJ),
            b"/proc/self/std/out\0",
            false,
            true,
        );
        init_std(
            core::ptr::addr_of_mut!(STDERR_OBJ),
            b"/proc/self/std/err\0",
            false,
            true,
        );
        init_std(
            core::ptr::addr_of_mut!(STDIN_OBJ),
            b"/proc/self/std/in\0",
            true,
            false,
        );
        // Standard streams are append-only ring buffers with no offsets, so
        // `fwrite` must use the append flag (bit 0), never positioned writes.
        // Without this, the first byte of a `printf` succeeds and every
        // subsequent chunk (offset in bits 8..63) fails, truncating output.
        STDOUT_OBJ.append = true;
        STDERR_OBJ.append = true;
        stdout = core::ptr::addr_of_mut!(STDOUT_OBJ);
        stderr = core::ptr::addr_of_mut!(STDERR_OBJ);
        stdin = core::ptr::addr_of_mut!(STDIN_OBJ);
    }
}

// ── FILE* over VFS paths ──────────────────────────────────────────────

/// A path-addressed file handle. `fread`/`fwrite` use the VFS file object's
/// `arg4` flags: read-at offset, append (bit 0) or positioned write (offset
/// in bits 8..63). No buffering; each call is one (chunked) syscall.
/// `ungetc` is a single-byte pushback (`-1` = empty).
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
    eof: bool,
    error: bool,
    pushback: c_int,
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
    eof: false,
    error: false,
    pushback: -1,
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
        // Resolve the path against the CWD into the FILE's own buffer.
        let Some(plen) = crate::vfs::resolve_c(path, &mut f.path) else {
            FILE_USED[i] = false;
            errno::set(36); // ENAMETOOLONG
            return ptr::null_mut();
        };
        f.plen = plen;
        f.offset = 0;
        f.readable = readable;
        f.writable = writable;
        f.append = append;
        f.slot = i;
        f.eof = false;
        f.error = false;
        f.pushback = -1;
        // C stdio open semantics: 'w'/'w+'/'a'/'a+' create a missing file;
        // 'r'/'r+' require it to exist (ENOENT otherwise).
        let exists = crate::vfs::stat_rs(&f.path[..plen]).is_ok();
        if !exists {
            if writable && (truncate || append) {
                if crate::vfs::create_rs(&f.path[..plen]) < 0 {
                    FILE_USED[i] = false;
                    return ptr::null_mut();
                }
            } else {
                FILE_USED[i] = false;
                errno::set(2); // ENOENT
                return ptr::null_mut();
            }
        }
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
        let mut got = 0usize;
        // Serve pushback byte first if present.
        if fref.pushback != -1 && want > 0 {
            *(buf as *mut u8) = (fref.pushback & 0xFF) as u8;
            fref.pushback = -1;
            fref.eof = false;
            got += 1;
            if want == 1 {
                return got / size;
            }
        }
        let path = fref.path;
        let plen = fref.plen;
        let data = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(FREAD_SCRATCH) as *mut u8,
            crate::IO_CHUNK_BYTES,
        );
        while got < want {
            let chunk = core::cmp::min(want - got, data.len());
            let r = read_path(&path[..plen + 1], &mut data[..chunk], fref.offset);
            if r < 0 {
                errno::set((-r) as c_int);
                fref.error = true;
                break;
            }
            let n = r as usize;
            if n == 0 {
                fref.eof = true;
                break;
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (buf as *mut u8).add(got), n);
            got += n;
            fref.offset += n as u64;
            if n < chunk {
                fref.eof = true;
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
                fref.error = true;
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
        // Seeking discards pushback and clears EOF.
        fref.pushback = -1;
        fref.eof = false;
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

// ── Status flags ──────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn feof(f: *mut FILE) -> c_int {
    if f.is_null() {
        return 0;
    }
    unsafe { (&*f).eof as c_int }
}

#[unsafe(no_mangle)]
pub extern "C" fn ferror(f: *mut FILE) -> c_int {
    if f.is_null() {
        return 0;
    }
    unsafe { (&*f).error as c_int }
}

#[unsafe(no_mangle)]
pub extern "C" fn clearerr(f: *mut FILE) {
    if f.is_null() {
        return;
    }
    unsafe {
        (&mut *f).eof = false;
        (&mut *f).error = false;
        (&mut *f).pushback = -1;
    }
}

// ── Character / line I/O ──────────────────────────────────────────────

/// Read one byte from the stream; returns the byte or `EOF` (-1).
#[unsafe(no_mangle)]
pub extern "C" fn fgetc(f: *mut FILE) -> c_int {
    if f.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        let fref = &mut *f;
        if fref.pushback != -1 {
            let c = fref.pushback;
            fref.pushback = -1;
            fref.eof = false;
            return c & 0xFF;
        }
    }
    let mut b = [0u8; 1];
    let n = fread(b.as_mut_ptr() as *mut c_void, 1, 1, f);
    if n == 1 { b[0] as c_int } else { -1 }
}

/// `ungetc(c, f)` — push one byte back; returns `c` or `EOF`.
#[unsafe(no_mangle)]
pub extern "C" fn ungetc(c: c_int, f: *mut FILE) -> c_int {
    if f.is_null() || c == -1 {
        return -1;
    }
    unsafe {
        let fref = &mut *f;
        if fref.pushback != -1 {
            return -1; // only one byte of pushback supported
        }
        fref.pushback = c & 0xFF;
        fref.eof = false;
        c & 0xFF
    }
}

/// Write one byte to the stream; returns the byte or `EOF`.
#[unsafe(no_mangle)]
pub extern "C" fn fputc(c: c_int, f: *mut FILE) -> c_int {
    let b = [(c & 0xFF) as u8; 1];
    if fwrite(b.as_ptr() as *const c_void, 1, 1, f) == 1 {
        c
    } else {
        -1
    }
}

/// `getc` — function form of `fgetc`.
#[unsafe(no_mangle)]
pub extern "C" fn getc(f: *mut FILE) -> c_int {
    fgetc(f)
}

/// `putc` — function form of `fputc`.
#[unsafe(no_mangle)]
pub extern "C" fn putc(c: c_int, f: *mut FILE) -> c_int {
    fputc(c, f)
}

/// `getchar` — read one byte from stdin (`/proc/self/std/in`).
#[unsafe(no_mangle)]
pub extern "C" fn getchar() -> c_int {
    let mut b = [0u8; 1];
    let r = unsafe { read_path(b"/proc/self/std/in\0", &mut b, 0) };
    if r > 0 {
        b[0] as c_int
    } else {
        errno::set((-(r)) as c_int);
        -1
    }
}

/// Read a line into `s` (at most `n-1` bytes, up to and including `\n`),
/// NUL-terminated. Returns `s` or `NULL` at EOF with nothing read.
#[unsafe(no_mangle)]
pub extern "C" fn fgets(s: *mut c_char, n: c_int, f: *mut FILE) -> *mut c_char {
    if s.is_null() || f.is_null() || n <= 0 {
        errno::set(errno::EINVAL);
        return core::ptr::null_mut();
    }
    let cap = n as usize;
    let mut i = 0usize;
    while i + 1 < cap {
        let c = fgetc(f);
        if c == -1 {
            if feof(f) != 0 && i > 0 {
                break;
            }
            if i == 0 {
                return core::ptr::null_mut();
            }
            break;
        }
        unsafe {
            *s.add(i) = (c & 0xFF) as c_char;
        }
        i += 1;
        if c == b'\n' as c_int {
            break;
        }
    }
    unsafe {
        *s.add(i) = 0;
    }
    s
}

/// Write a NUL-terminated string (no trailing newline) to the stream.
#[unsafe(no_mangle)]
pub extern "C" fn fputs(s: *const c_char, f: *mut FILE) -> c_int {
    if s.is_null() || f.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    let len = unsafe { crate::string::strlen(s) };
    if fwrite(s as *const c_void, 1, len, f) == len {
        0
    } else {
        -1
    }
}

/// `rewind(f)` — seek to the start and clear the EOF flag.
#[unsafe(no_mangle)]
pub extern "C" fn rewind(f: *mut FILE) {
    if f.is_null() {
        return;
    }
    unsafe {
        (&mut *f).offset = 0;
        (&mut *f).eof = false;
        (&mut *f).error = false;
        (&mut *f).pushback = -1;
    }
}

// ── POSIX extras ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn fseeko(f: *mut FILE, offset: c_long, whence: c_int) -> c_int {
    fseek(f, offset, whence)
}

#[unsafe(no_mangle)]
pub extern "C" fn ftello(f: *mut FILE) -> c_long {
    ftell(f)
}

#[allow(non_camel_case_types)]
pub type fpos_t = c_long;

#[unsafe(no_mangle)]
pub extern "C" fn fgetpos(f: *mut FILE, pos: *mut fpos_t) -> c_int {
    if f.is_null() || pos.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        *pos = (*f).offset as fpos_t;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn fsetpos(f: *mut FILE, pos: *const fpos_t) -> c_int {
    if f.is_null() || pos.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        let off = *pos;
        fseek(f, off, 0)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fileno(f: *mut FILE) -> c_int {
    if f.is_null() {
        errno::set(errno::EBADF);
        return -1;
    }
    unsafe {
        if f == stdin {
            return 0;
        }
        if f == stdout {
            return 1;
        }
        if f == stderr {
            return 2;
        }
        // Search fd table for matching path (path equality).
        for i in 3..32 {
            let path_slice = core::slice::from_raw_parts((*f).path.as_ptr(), (*f).plen);
            let fd_ok = crate::fd::fileno_path(i as c_int, path_slice);
            if fd_ok {
                return i as c_int;
            }
        }
        errno::set(errno::EBADF);
        -1
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fdopen(fd: c_int, mode: *const c_char) -> *mut FILE {
    if mode.is_null() {
        errno::set(errno::EINVAL);
        return ptr::null_mut();
    }
    // Validate fd exists.
    let is_std = fd >= 0 && fd <= 2;
    let path_opt: Option<[u8; 128]> = if is_std {
        let p: &[u8] = match fd {
            0 => b"/proc/self/std/in\0",
            1 => b"/proc/self/std/out\0",
            _ => b"/proc/self/std/err\0",
        };
        let mut arr = [0u8; 128];
        arr[..p.len()].copy_from_slice(p);
        Some(arr)
    } else {
        crate::fd::fd_path(fd)
    };
    let Some(path_arr) = path_opt else {
        errno::set(errno::EBADF);
        return ptr::null_mut();
    };
    let mlen = unsafe { crate::string::strlen(mode) };
    let m = unsafe { core::slice::from_raw_parts(mode as *const u8, mlen) };
    let Some((readable, writable, append, _trunc)) = parse_mode(m) else {
        errno::set(errno::EINVAL);
        return ptr::null_mut();
    };
    unsafe {
        let mut slot = None;
        for i in 0..FILE_POOL {
            if !FILE_USED[i] {
                slot = Some(i);
                break;
            }
        }
        let Some(i) = slot else {
            errno::set(errno::EMFILE);
            return ptr::null_mut();
        };
        FILE_USED[i] = true;
        let f = &mut FILES[i];
        *f = FILE_INIT;
        f.path = path_arr;
        // compute plen: find NUL
        let mut plen = 0usize;
        while plen < 128 && f.path[plen] != 0 {
            plen += 1;
        }
        f.plen = plen;
        f.readable = readable;
        f.writable = writable;
        f.append = append;
        f.slot = i;
        f as *mut FILE
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn freopen(path: *const c_char, mode: *const c_char, f: *mut FILE) -> *mut FILE {
    if f.is_null() || path.is_null() || mode.is_null() {
        errno::set(errno::EINVAL);
        return ptr::null_mut();
    }
    fclose(f);
    let newf = fopen(path, mode);
    if newf.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        // Reuse the original pointer by copying.
        *f = *newf;
        let idx = (*f).slot;
        // free the temp slot if different
        let new_idx = (*newf).slot;
        if new_idx < FILE_POOL && new_idx != idx {
            FILE_USED[new_idx] = false;
        }
        (*f).slot = idx;
        FILE_USED[idx] = true;
    }
    f
}

#[unsafe(no_mangle)]
pub extern "C" fn setbuf(f: *mut FILE, _buf: *mut c_char) {
    let _ = f;
}

#[unsafe(no_mangle)]
pub extern "C" fn setvbuf(_f: *mut FILE, _buf: *mut c_char, _mode: c_int, _size: usize) -> c_int {
    0
}

static mut TMP_COUNTER: u64 = 0;

#[unsafe(no_mangle)]
pub extern "C" fn tmpfile() -> *mut FILE {
    unsafe {
        let n = TMP_COUNTER;
        TMP_COUNTER = TMP_COUNTER.wrapping_add(1);
        let mut name = [0u8; 64];
        let prefix = b"/A/tmpfile_";
        name[..prefix.len()].copy_from_slice(prefix);
        let mut len = prefix.len();
        // hex counter
        let mut v = n;
        if v == 0 {
            name[len] = b'0';
            len += 1;
        } else {
            let mut tmp = [0u8; 16];
            let mut tl = 0usize;
            while v > 0 {
                tmp[tl] = b"0123456789abcdef"[(v & 0xF) as usize];
                tl += 1;
                v >>= 4;
            }
            for i in (0..tl).rev() {
                name[len] = tmp[i];
                len += 1;
            }
        }
        name[len] = 0;
        // create then open w+
        let mut tmp_arr = [0i8; 64];
        core::ptr::copy_nonoverlapping(name.as_ptr() as *const i8, tmp_arr.as_mut_ptr(), len + 1);
        let f = fopen(tmp_arr.as_ptr() as *const c_char, c"w+".as_ptr());
        if !f.is_null() {
            crate::vfs::unlink(tmp_arr.as_ptr() as *const c_char);
        }
        f
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tmpnam(s: *mut c_char) -> *mut c_char {
    static mut TMPNAM_BUF: [u8; 64] = [0; 64];
    unsafe {
        let buf = if s.is_null() {
            core::ptr::addr_of_mut!(TMPNAM_BUF) as *mut c_char
        } else {
            s
        };
        let n = TMP_COUNTER;
        TMP_COUNTER = TMP_COUNTER.wrapping_add(1);
        let mut tmp = [0u8; 64];
        let prefix = b"/A/tmpnam_";
        tmp[..prefix.len()].copy_from_slice(prefix);
        let mut len = prefix.len();
        let mut v = n;
        if v == 0 {
            tmp[len] = b'0';
            len += 1;
        } else {
            let mut hex = [0u8; 16];
            let mut hl = 0usize;
            while v > 0 {
                hex[hl] = b"0123456789abcdef"[(v & 0xF) as usize];
                hl += 1;
                v >>= 4;
            }
            for i in (0..hl).rev() {
                tmp[len] = hex[i];
                len += 1;
            }
        }
        tmp[len] = 0;
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf as *mut u8, len + 1);
        buf
    }
}

/// `getdelim` — read until `delim` (inclusive) or EOF, growing `*lineptr`.
#[unsafe(no_mangle)]
pub extern "C" fn getdelim(
    lineptr: *mut *mut c_char,
    n: *mut usize,
    delim: c_int,
    f: *mut FILE,
) -> isize {
    if lineptr.is_null() || n.is_null() || f.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    let d = (delim & 0xFF) as u8;
    unsafe {
        let mut cap = *n;
        let mut ptr = *lineptr;
        let mut len = 0usize;
        if ptr.is_null() || cap == 0 {
            cap = 128;
            ptr = crate::mem::malloc(cap) as *mut c_char;
            if ptr.is_null() {
                errno::set(errno::ENOMEM);
                return -1;
            }
            *lineptr = ptr;
            *n = cap;
        }
        loop {
            let c = fgetc(f);
            if c == -1 {
                if len == 0 {
                    return -1;
                }
                break;
            }
            if len + 2 > cap {
                let newcap = cap.saturating_mul(2).max(128);
                let np = crate::mem::realloc(ptr as *mut core::ffi::c_void, newcap) as *mut c_char;
                if np.is_null() {
                    errno::set(errno::ENOMEM);
                    return -1;
                }
                ptr = np;
                cap = newcap;
                *lineptr = ptr;
                *n = cap;
            }
            *ptr.add(len) = (c & 0xFF) as c_char;
            len += 1;
            if (c & 0xFF) as u8 == d {
                break;
            }
        }
        if len < cap {
            *ptr.add(len) = 0;
        } else if len == cap {
            // need one more for NUL
            let newcap = cap + 1;
            let np = crate::mem::realloc(ptr as *mut core::ffi::c_void, newcap) as *mut c_char;
            if !np.is_null() {
                *np.add(len) = 0;
                *lineptr = np;
                *n = newcap;
            }
        }
        len as isize
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn getline(lineptr: *mut *mut c_char, n: *mut usize, f: *mut FILE) -> isize {
    getdelim(lineptr, n, b'\n' as c_int, f)
}

struct FdSink {
    fd: c_int,
}
impl crate::format::Sink for FdSink {
    fn write(&mut self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        let _ = crate::fd::write_fd(self.fd, b.as_ptr() as *const c_void, b.len());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vdprintf(fd: c_int, fmt: *const c_char, ap: core::ffi::VaList) -> c_int {
    unsafe {
        let mut sink = FdSink { fd };
        crate::format::format_to_sink(&mut sink, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dprintf(fd: c_int, fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let ap = args;
        vdprintf(fd, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vasprintf(strp: *mut *mut c_char, fmt: *const c_char, ap: core::ffi::VaList) -> c_int {
    if strp.is_null() || fmt.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        let mut sink = crate::format::HeapSink {
            ptr: core::ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        let r = crate::format::format_to_sink(&mut sink, fmt, ap);
        if r < 0 {
            if !sink.ptr.is_null() {
                crate::mem::free(sink.ptr as *mut core::ffi::c_void);
            }
            return -1;
        }
        // NUL-terminate
        if sink.len + 1 > sink.cap {
            let np = crate::mem::realloc(sink.ptr as *mut core::ffi::c_void, sink.len + 1);
            if np.is_null() {
                crate::mem::free(sink.ptr as *mut core::ffi::c_void);
                errno::set(errno::ENOMEM);
                return -1;
            }
            sink.ptr = np as *mut u8;
            sink.cap = sink.len + 1;
        }
        *sink.ptr.add(sink.len) = 0;
        *strp = sink.ptr as *mut c_char;
        // Prevent double-free of sink.ptr on drop; we transferred ownership.
        core::mem::forget(sink);
        r
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn asprintf(strp: *mut *mut c_char, fmt: *const c_char, args: ...) -> c_int {
    unsafe {
        let ap = args;
        vasprintf(strp, fmt, ap)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn flockfile(_f: *mut FILE) {}
#[unsafe(no_mangle)]
pub extern "C" fn ftrylockfile(_f: *mut FILE) -> c_int { 0 }
#[unsafe(no_mangle)]
pub extern "C" fn funlockfile(_f: *mut FILE) {}
