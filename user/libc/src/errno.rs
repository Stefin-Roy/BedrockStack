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
pub const ENOTBLK: c_int = 15;
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
pub const ETXTBSY: c_int = 26;
pub const EFBIG: c_int = 27;
pub const ENOSPC: c_int = 28;
pub const ESPIPE: c_int = 29;
pub const EROFS: c_int = 30;
pub const EMLINK: c_int = 31;
pub const EPIPE: c_int = 32;
pub const EDOM: c_int = 33;
pub const ERANGE: c_int = 34;
pub const EDEADLK: c_int = 35;
pub const EDEADLOCK: c_int = 35;
pub const ENAMETOOLONG: c_int = 36;
pub const ENOLCK: c_int = 37;
pub const ENOSYS: c_int = 38;
pub const ENOTEMPTY: c_int = 39;
pub const ELOOP: c_int = 40;
pub const EWOULDBLOCK: c_int = 11;
pub const ENOMSG: c_int = 42;
pub const EIDRM: c_int = 43;
pub const ECHRNG: c_int = 44;
pub const EL2NSYNC: c_int = 45;
pub const EL3HLT: c_int = 46;
pub const EL3RST: c_int = 47;
pub const ELNRNG: c_int = 48;
pub const EUNATCH: c_int = 49;
pub const ENOCSI: c_int = 50;
pub const EL2HLT: c_int = 51;
pub const EBADE: c_int = 52;
pub const EBADR: c_int = 53;
pub const EXFULL: c_int = 54;
pub const ENOANO: c_int = 55;
pub const EBADRQC: c_int = 56;
pub const EBADSLT: c_int = 57;
pub const EBFONT: c_int = 59;
pub const ENOSTR: c_int = 60;
pub const ENODATA: c_int = 61;
pub const ETIME: c_int = 62;
pub const ENOSR: c_int = 63;
pub const ENONET: c_int = 64;
pub const ENOPKG: c_int = 65;
pub const EREMOTE: c_int = 66;
pub const ENOLINK: c_int = 67;
pub const EADV: c_int = 68;
pub const ESRMNT: c_int = 69;
pub const ECOMM: c_int = 70;
pub const EPROTO: c_int = 71;
pub const EMULTIHOP: c_int = 72;
pub const EDOTDOT: c_int = 73;
pub const EBADMSG: c_int = 74;
pub const EOVERFLOW: c_int = 75;
pub const ENOTUNIQ: c_int = 76;
pub const EBADFD: c_int = 77;
pub const EREMCHG: c_int = 78;
pub const ELIBACC: c_int = 79;
pub const ELIBBAD: c_int = 80;
pub const ELIBSCN: c_int = 81;
pub const ELIBMAX: c_int = 82;
pub const ELIBEXEC: c_int = 83;
pub const EILSEQ: c_int = 84;
pub const ERESTART: c_int = 85;
pub const ESTRPIPE: c_int = 86;
pub const EUSERS: c_int = 87;
pub const ENOTSOCK: c_int = 88;
pub const EDESTADDRREQ: c_int = 89;
pub const EMSGSIZE: c_int = 90;
pub const EPROTOTYPE: c_int = 91;
pub const ENOPROTOOPT: c_int = 92;
pub const EPROTONOSUPPORT: c_int = 93;
pub const ESOCKTNOSUPPORT: c_int = 94;
pub const EOPNOTSUPP: c_int = 95;
pub const EPFNOSUPPORT: c_int = 96;
pub const EAFNOSUPPORT: c_int = 97;
pub const EADDRINUSE: c_int = 98;
pub const EADDRNOTAVAIL: c_int = 99;
pub const ENETDOWN: c_int = 100;
pub const ENETUNREACH: c_int = 101;
pub const ENETRESET: c_int = 102;
pub const ECONNABORTED: c_int = 103;
pub const ECONNRESET: c_int = 104;
pub const ENOBUFS: c_int = 105;
pub const EISCONN: c_int = 106;
pub const ENOTCONN: c_int = 107;
pub const ESHUTDOWN: c_int = 108;
pub const ETOOMANYREFS: c_int = 109;
pub const ETIMEDOUT: c_int = 110;
pub const ECONNREFUSED: c_int = 111;
pub const EHOSTDOWN: c_int = 112;
pub const EHOSTUNREACH: c_int = 113;
pub const EALREADY: c_int = 114;
pub const EINPROGRESS: c_int = 115;
pub const ESTALE: c_int = 116;
pub const EUCLEAN: c_int = 117;
pub const ENOTNAM: c_int = 118;
pub const ENAVAIL: c_int = 119;
pub const EISNAM: c_int = 120;
pub const EREMOTEIO: c_int = 121;
pub const EDQUOT: c_int = 122;
pub const ENOMEDIUM: c_int = 123;
pub const EMEDIUMTYPE: c_int = 124;
pub const ECANCELED: c_int = 125;
pub const ENOKEY: c_int = 126;
pub const EKEYEXPIRED: c_int = 127;
pub const EKEYREVOKED: c_int = 128;
pub const EKEYREJECTED: c_int = 129;
pub const EOWNERDEAD: c_int = 130;
pub const ENOTRECOVERABLE: c_int = 131;
pub const ERFKILL: c_int = 132;
pub const EHWPOISON: c_int = 133;
pub const ENOTSUP: c_int = 95;

#[unsafe(no_mangle)]
pub extern "C" fn __errno_location() -> *mut c_int {
    core::ptr::addr_of_mut!(ERRNO)
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
        ELOOP => b"Too many levels of symbolic links",
        ENOMSG => b"No message of desired type",
        EIDRM => b"Identifier removed",
        ECHRNG => b"Channel number out of range",
        EL2NSYNC => b"Level 2 not synchronized",
        EL3HLT => b"Level 3 halted",
        EL3RST => b"Level 3 reset",
        ELNRNG => b"Link number out of range",
        EUNATCH => b"Protocol driver not attached",
        ENOCSI => b"No CSI structure available",
        EL2HLT => b"Level 2 halted",
        EBADE => b"Invalid exchange",
        EBADR => b"Invalid request descriptor",
        EXFULL => b"Exchange full",
        ENOANO => b"No anode",
        EBADRQC => b"Invalid request code",
        EBADSLT => b"Invalid slot",
        EBFONT => b"Bad font file format",
        ENOSTR => b"Device not a stream",
        ENODATA => b"No data available",
        ETIME => b"Timer expired",
        ENOSR => b"Out of streams resources",
        ENONET => b"Machine is not on the network",
        ENOPKG => b"Package not installed",
        EREMOTE => b"Object is remote",
        ENOLINK => b"Link has been severed",
        EADV => b"Advertise error",
        ESRMNT => b"Srmount error",
        ECOMM => b"Communication error on send",
        EPROTO => b"Protocol error",
        EMULTIHOP => b"Multihop attempted",
        EDOTDOT => b"RFS specific error",
        EBADMSG => b"Bad message",
        EOVERFLOW => b"Value too large for defined data type",
        ENOTUNIQ => b"Name not unique on network",
        EBADFD => b"File descriptor in bad state",
        EREMCHG => b"Remote address changed",
        ELIBACC => b"Can not access a needed shared library",
        ELIBBAD => b"Accessing a corrupted shared library",
        ELIBSCN => b".lib section in a.out corrupted",
        ELIBMAX => b"Attempting to link in too many shared libraries",
        ELIBEXEC => b"Cannot exec a shared library directly",
        EILSEQ => b"Illegal byte sequence",
        ERESTART => b"Interrupted system call should be restarted",
        ESTRPIPE => b"Streams pipe error",
        EUSERS => b"Too many users",
        ENOTSOCK => b"Socket operation on non-socket",
        EDESTADDRREQ => b"Destination address required",
        EMSGSIZE => b"Message too long",
        EPROTOTYPE => b"Protocol wrong type for socket",
        ENOPROTOOPT => b"Protocol not available",
        EPROTONOSUPPORT => b"Protocol not supported",
        ESOCKTNOSUPPORT => b"Socket type not supported",
        EAFNOSUPPORT => b"Address family not supported by protocol",
        EADDRINUSE => b"Address already in use",
        EADDRNOTAVAIL => b"Cannot assign requested address",
        ENETDOWN => b"Network is down",
        ENETUNREACH => b"Network is unreachable",
        ENETRESET => b"Network dropped connection on reset",
        ECONNABORTED => b"Software caused connection abort",
        ECONNRESET => b"Connection reset by peer",
        ENOBUFS => b"No buffer space available",
        EISCONN => b"Transport endpoint is already connected",
        ENOTCONN => b"Transport endpoint is not connected",
        ESHUTDOWN => b"Cannot send after transport endpoint shutdown",
        ETOOMANYREFS => b"Too many references: cannot splice",
        ETIMEDOUT => b"Connection timed out",
        ECONNREFUSED => b"Connection refused",
        EHOSTDOWN => b"Host is down",
        EHOSTUNREACH => b"No route to host",
        EALREADY => b"Operation already in progress",
        EINPROGRESS => b"Operation now in progress",
        ESTALE => b"Stale file handle",
        EUCLEAN => b"Structure needs cleaning",
        ENOTNAM => b"Not a XENIX named type file",
        ENAVAIL => b"No XENIX semaphores available",
        EISNAM => b"Is a named type file",
        EREMOTEIO => b"Remote I/O error",
        EDQUOT => b"Disk quota exceeded",
        ENOMEDIUM => b"No medium found",
        EMEDIUMTYPE => b"Wrong medium type",
        ECANCELED => b"Operation canceled",
        ENOKEY => b"Required key not available",
        EKEYEXPIRED => b"Key has expired",
        EKEYREVOKED => b"Key has been revoked",
        EKEYREJECTED => b"Key was rejected by service",
        EOWNERDEAD => b"Owner died",
        ENOTRECOVERABLE => b"State not recoverable",
        ERFKILL => b"Operation not possible due to RF-kill",
        EHWPOISON => b"Memory page has hardware error",
        ETXTBSY => b"Text file busy",
        EDEADLK => b"Resource deadlock avoided",
        ENOTBLK => b"Block device required",
        ENOLCK => b"No locks available",
        // extra
        _ => b"Unknown error",
    }
}

/// `strerror(err)` — returns a pointer to a static NUL-terminated message.
#[unsafe(no_mangle)]
pub extern "C" fn strerror(err: c_int) -> *const c_char {
    let m = msg(err);
    static mut BUF: [u8; 128] = [0; 128];
    unsafe {
        let ptr = core::ptr::addr_of_mut!(BUF) as *mut u8;
        let n = core::cmp::min(m.len(), 127);
        core::ptr::copy_nonoverlapping(m.as_ptr(), ptr, n);
        *ptr.add(n) = 0;
        ptr as *const c_char
    }
}

/// `strerror_r(err, buf, buflen)` — XSI-compliant. Copies message into `buf`,
/// NUL-terminates, returns 0 on success or ERANGE if truncated.
#[unsafe(no_mangle)]
pub extern "C" fn strerror_r(err: c_int, buf: *mut c_char, buflen: usize) -> c_int {
    if buf.is_null() || buflen == 0 {
        return crate::errno::EINVAL;
    }
    let m = msg(err);
    let n = core::cmp::min(m.len(), buflen - 1);
    unsafe {
        core::ptr::copy_nonoverlapping(m.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    if m.len() >= buflen {
        crate::errno::ERANGE
    } else {
        0
    }
}

/// `perror(s)` — print `s: strerror(errno)` to stderr if `s` non-empty.
#[unsafe(no_mangle)]
pub extern "C" fn perror(s: *const c_char) {
    let err = get();
    let m = msg(err);
    let has_prefix = !s.is_null() && unsafe { *s != 0 };
    // Build into a small stack buf then write to stderr via syscall scratch.
    let mut tmp = [0u8; 256];
    let mut len = 0usize;
    if has_prefix {
        let slen = unsafe { crate::string::strlen(s) };
        let n = core::cmp::min(slen, tmp.len() - 3 - m.len());
        unsafe {
            core::ptr::copy_nonoverlapping(s as *const u8, tmp.as_mut_ptr(), n);
            len += n;
            tmp[len] = b':';
            len += 1;
            tmp[len] = b' ';
            len += 1;
        }
    }
    let n = core::cmp::min(m.len(), tmp.len() - len - 1);
    tmp[len..len + n].copy_from_slice(&m[..n]);
    len += n;
    if len < tmp.len() {
        tmp[len] = b'\n';
        len += 1;
    }
    let _ = crate::syscall::write_data(b"/proc/self/std/err\0", &tmp[..len], 0);
}
