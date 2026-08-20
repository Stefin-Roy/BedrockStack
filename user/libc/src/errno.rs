use core::ffi::{c_char, c_int};

static mut ERRNO: c_int = 0;

// ── POSIX errno values (Linux x86_64 numbers) ─────────────────────────

pub const EPERM: c_int = 1;
pub const ENOENT: c_int = 2;
pub const ESRCH: c_int = 3;
pub const EINTR: c_int = 4;
pub const EIO: c_int = 5;
pub const ENXIO: c_int = 6;
pub const E2BIG: c_int = 7;
pub const ENOEXEC: c_int = 8;
pub const EBADF: c_int = 9;
pub const ECHILD: c_int = 10;
pub const EAGAIN: c_int = 11;
pub const ENOMEM: c_int = 12;
pub const EACCES: c_int = 13;
pub const EFAULT: c_int = 14;
pub const EBUSY: c_int = 16;
pub const EEXIST: c_int = 17;
pub const EXDEV: c_int = 18;
pub const ENODEV: c_int = 19;
pub const ENOTDIR: c_int = 20;
pub const EISDIR: c_int = 21;
pub const EINVAL: c_int = 22;
pub const ENFILE: c_int = 23;
pub const EMFILE: c_int = 24;
pub const ENOTTY: c_int = 25;
pub const EFBIG: c_int = 27;
pub const ENOSPC: c_int = 28;
pub const ESPIPE: c_int = 29;
pub const EROFS: c_int = 30;
pub const EMLINK: c_int = 31;
pub const EPIPE: c_int = 32;
pub const EDOM: c_int = 33;
pub const ERANGE: c_int = 34;
pub const ENAMETOOLONG: c_int = 36;
pub const ENOSYS: c_int = 38;
pub const ENOTEMPTY: c_int = 39;

#[unsafe(no_mangle)]
pub extern "C" fn __errno_location() -> *mut c_int {
    unsafe { core::ptr::addr_of_mut!(ERRNO) }
}

/// Convert a raw syscall return: if negative, store -ret as errno and return
/// -1, else return the positive value unchanged.
pub fn ret(ret: isize) -> isize {
    if ret < 0 {
        unsafe {
            ERRNO = (-ret) as c_int;
        }
        -1
    } else {
        ret
    }
}

/// Set errno directly.
pub fn set(err: c_int) {
    unsafe {
        ERRNO = err;
    }
}

/// Read the current errno.
pub fn get() -> c_int {
    unsafe { ERRNO }
}

fn msg(err: c_int) -> &'static [u8] {
    match err {
        EPERM => b"Operation not permitted",
        ENOENT => b"No such file or directory",
        ESRCH => b"No such process",
        EINTR => b"Interrupted system call",
        EIO => b"I/O error",
        ENXIO => b"No such device or address",
        E2BIG => b"Argument list too long",
        ENOEXEC => b"Exec format error",
        EBADF => b"Bad file descriptor",
        ECHILD => b"No child processes",
        EAGAIN => b"Resource temporarily unavailable",
        ENOMEM => b"Cannot allocate memory",
        EACCES => b"Permission denied",
        EFAULT => b"Bad address",
        EBUSY => b"Device or resource busy",
        EEXIST => b"File exists",
        EXDEV => b"Invalid cross-device link",
        ENODEV => b"No such device",
        ENOTDIR => b"Not a directory",
        EISDIR => b"Is a directory",
        EINVAL => b"Invalid argument",
        ENFILE => b"Too many open files in system",
        EMFILE => b"Too many open files",
        ENOTTY => b"Inappropriate ioctl for device",
        EFBIG => b"File too large",
        ENOSPC => b"No space left on device",
        ESPIPE => b"Illegal seek",
        EROFS => b"Read-only file system",
        EMLINK => b"Too many links",
        EPIPE => b"Broken pipe",
        EDOM => b"Numerical argument out of domain",
        ERANGE => b"Numerical result out of range",
        ENAMETOOLONG => b"File name too long",
        ENOSYS => b"Function not implemented",
        ENOTEMPTY => b"Directory not empty",
        _ => b"Unknown error",
    }
}

/// `strerror(err)` — returns a pointer to a static NUL-terminated message.
#[unsafe(no_mangle)]
pub extern "C" fn strerror(err: c_int) -> *const c_char {
    let m = msg(err);
    static mut BUF: [u8; 64] = [0; 64];
    unsafe {
        let ptr = core::ptr::addr_of_mut!(BUF) as *mut u8;
        let n = core::cmp::min(m.len(), 63);
        core::ptr::copy_nonoverlapping(m.as_ptr(), ptr, n);
        *ptr.add(n) = 0;
        ptr as *const c_char
    }
}