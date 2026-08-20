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
    if fd <= 2 {
        1
    } else {
        0
    }
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
        _SC_PAGESIZE | _SC_PAGE_SIZE => 4096,
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
        let mut set = |dst: &mut [c_char; 65], src: &[u8]| {
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