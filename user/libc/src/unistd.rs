//! `<unistd.h>` / `<sys/utsname.h>` — identity, resource, and process stubs
//! backed by what the unispace kernel offers.  There is no permission model,
//! so uid/gid are 0 and `access` grants every permission to existing paths.

use core::ffi::{c_char, c_int, c_long};

use crate::errno;
use crate::syscall::read_path;

// ── identity (no security model) ──────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn getuid() -> c_int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn geteuid() -> c_int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn getgid() -> c_int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn getegid() -> c_int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn setuid(_u: c_int) -> c_int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn setgid(_g: c_int) -> c_int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn getgroups(_size: c_int, _list: *mut c_int) -> c_int {
    0
}

// ── terminals & access ────────────────────────────────────────────────

/// `isatty(fd)` — the std streams are the task console; file fds are not.
#[unsafe(no_mangle)]
pub extern "C" fn isatty(fd: c_int) -> c_int {
    if fd <= 2 { 1 } else { 0 }
}

/// `access(path, mode)` — F_OK checks existence; R/W/X are always granted.
#[unsafe(no_mangle)]
pub extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    if path.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    if mode == 0 {
        // F_OK
        let p =
            unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) };
        match crate::vfs::stat_rs(p) {
            Ok(_) => 0,
            Err(e) => {
                errno::set(e);
                -1
            }
        }
    } else {
        0
    }
}

// ── sysconf ───────────────────────────────────────────────────────────

pub const _SC_PAGESIZE: c_int = 30;
pub const _SC_PAGE_SIZE: c_int = 30;
pub const _SC_NPROCESSORS_CONF: c_int = 83;
pub const _SC_NPROCESSORS_ONLN: c_int = 84;
pub const _SC_PHYS_PAGES: c_int = 85;

/// `sysconf(name)` — a small set of fixed answers.
#[unsafe(no_mangle)]
pub extern "C" fn sysconf(name: c_int) -> c_long {
    match name {
        _SC_PAGESIZE => 4096, // _SC_PAGE_SIZE == _SC_PAGESIZE (30)
        _SC_NPROCESSORS_CONF | _SC_NPROCESSORS_ONLN => {
            let mut buf = [0u8; 4];
            let r = unsafe { read_path(b"/sys/cpus\0", &mut buf, 0) };
            if r < 4 {
                return -1;
            }
            u32::from_le_bytes(buf[0..4].try_into().unwrap()) as c_long
        }
        _ => {
            errno::set(errno::EINVAL);
            -1
        }
    }
}

// ── uname ─────────────────────────────────────────────────────────────

/// `struct utsname`
#[repr(C)]
pub struct Utsname {
    pub sysname: [c_char; 65],
    pub nodename: [c_char; 65],
    pub release: [c_char; 65],
    pub version: [c_char; 65],
    pub machine: [c_char; 65],
}

/// `uname(&utsname)` — reads `/sys/version` (a `str` wire) for the release.
#[unsafe(no_mangle)]
pub extern "C" fn uname(u: *mut Utsname) -> c_int {
    if u.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    let mut buf = [0u8; 256];
    let r = unsafe { read_path(b"/sys/version\0", &mut buf, 0) };
    let mut rel = [0u8; 65];
    if r >= 4 {
        let n = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let avail = (r as usize).saturating_sub(4);
        let n = core::cmp::min(n, core::cmp::min(avail, 64));
        rel[..n].copy_from_slice(&buf[4..4 + n]);
    } else {
        rel[0] = b'?';
    }
    unsafe {
        let set = |dst: &mut [c_char; 65], src: &[u8]| {
            let n = core::cmp::min(src.len(), 64);
            *dst = [0; 65];
            for i in 0..n {
                dst[i] = src[i] as c_char;
            }
        };
        set(&mut (*u).sysname, b"BedrockOS");
        set(&mut (*u).nodename, b"bedrock");
        set(&mut (*u).release, &rel[..64]);
        set(&mut (*u).version, b"#1 unispace");
        set(&mut (*u).machine, b"x86_64");
    }
    0
}

// ── process stubs ─────────────────────────────────────────────────────

/// `fork()` — the kernel has no fork; always fails.
#[unsafe(no_mangle)]
pub extern "C" fn fork() -> c_int {
    errno::set(errno::ENOSYS);
    -1
}

/// `execve()` — the kernel only spawns by path; always fails.
#[unsafe(no_mangle)]
pub extern "C" fn execve(
    _path: *const c_char,
    _argv: *const *const c_char,
    _envp: *const *const c_char,
) -> c_int {
    errno::set(errno::ENOSYS);
    -1
}

/// `_exit(code)` — terminates without any cleanup.
#[unsafe(no_mangle)]
pub extern "C" fn _exit(code: c_int) -> ! {
    crate::process::exit(code.max(0) as usize)
}

/// `pause()` — sleep indefinitely (approximated by a long sleep).
#[unsafe(no_mangle)]
pub extern "C" fn pause() -> c_int {
    crate::process::sleep_ms(u64::MAX);
    -1
}

// ── pathconf / confstr ──────────────────────────────────────────────

pub const _PC_NAME_MAX: c_int = 4;
pub const _PC_PATH_MAX: c_int = 5;
pub const _PC_PIPE_BUF: c_int = 6;

#[unsafe(no_mangle)]
pub extern "C" fn pathconf(_path: *const c_char, name: c_int) -> c_long {
    match name {
        _PC_NAME_MAX => 255,
        _PC_PATH_MAX => 4096,
        _PC_PIPE_BUF => 512,
        _ => {
            crate::errno::set(crate::errno::EINVAL);
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fpathconf(_fd: c_int, name: c_int) -> c_long {
    pathconf(core::ptr::null(), name)
}

#[unsafe(no_mangle)]
pub extern "C" fn confstr(_name: c_int, _buf: *mut c_char, _len: usize) -> usize {
    crate::errno::set(crate::errno::EINVAL);
    0
}

// ── gethostname ─────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn gethostname(name: *mut c_char, len: usize) -> c_int {
    if name.is_null() || len == 0 {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let host = b"bedrock";
    let n = core::cmp::min(host.len(), len - 1);
    unsafe {
        core::ptr::copy_nonoverlapping(host.as_ptr(), name as *mut u8, n);
        *name.add(n) = 0;
    }
    0
}

// ── getopt ──────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub static mut optind: c_int = 1;
#[unsafe(no_mangle)]
pub static mut optarg: *mut c_char = core::ptr::null_mut();
#[unsafe(no_mangle)]
pub static mut opterr: c_int = 1;
#[unsafe(no_mangle)]
pub static mut optopt: c_int = 0;

#[unsafe(no_mangle)]
pub extern "C" fn getopt(argc: c_int, argv: *const *const c_char, optstring: *const c_char) -> c_int {
    if argc <= 0 || argv.is_null() || optstring.is_null() {
        return -1;
    }
    unsafe {
        if optind as usize >= argc as usize {
            return -1;
        }
        let arg = *argv.add(optind as usize);
        if arg.is_null() || *arg == 0 || *arg as u8 != b'-' || *arg.add(1) == 0 {
            return -1;
        }
        if *arg.add(1) as u8 == b'-' {
            optind += 1;
            return -1;
        }
        let opt = *arg.add(1) as u8;
        optopt = opt as c_int;
        // Find in optstring
        let mut p = optstring;
        let mut found = false;
        let mut needs_arg = false;
        while *p != 0 {
            if *p as u8 == opt {
                found = true;
                if *p.add(1) as u8 == b':' {
                    needs_arg = true;
                }
                break;
            }
            p = p.add(1);
        }
        if !found {
            if opterr != 0 {
                crate::errno::set(crate::errno::EINVAL);
            }
            optind += 1;
            return b'?' as c_int;
        }
        if needs_arg {
            if optind + 1 < argc {
                let next = *argv.add((optind + 1) as usize);
                optarg = next as *mut c_char;
                optind += 2;
            } else {
                optind += 1;
                return b'?' as c_int;
            }
        } else {
            optind += 1;
        }
        opt as c_int
    }
}

// ── pipe / chown ───────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn pipe(_fds: *mut c_int) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn chown(_path: *const c_char, _owner: c_int, _group: c_int) -> c_int {
    0 // no ownership model — succeed
}

#[unsafe(no_mangle)]
pub extern "C" fn fchown(_fd: c_int, _owner: c_int, _group: c_int) -> c_int {
    0
}

// ── exec family stubs ───────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn execv(_path: *const c_char, _argv: *const *const c_char) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}
#[unsafe(no_mangle)]
pub extern "C" fn execvp(_file: *const c_char, _argv: *const *const c_char) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execl(_path: *const c_char, _arg: *const c_char, mut _args: ...) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execlp(_file: *const c_char, _arg: *const c_char, mut _args: ...) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}
