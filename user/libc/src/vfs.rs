//! Thin wrappers over the kernel's VFS unispace methods (`/A`, `/B`).
//!
//! The kernel's VFS objects expose directory methods (`create`/`mkdir`/
//! `rmdir`/`unlink`/`rename`) on directory objects and file methods
//! (`stat`/`truncate`) on file objects.  These helpers resolve a path into a
//! parent directory plus a basename and issue the method on the parent, since
//! the methods are addressed by object, not by full path.

use core::ffi::{c_char, c_int};

use crate::errno;
use crate::syscall::write_path;

// ── current working directory ─────────────────────────────────────────

/// Process-wide CWD, NUL-terminated. Defaults to `/`; set by `chdir`.
/// The kernel's VFS is absolute-path-only, so every path-taking wrapper
/// resolves relative paths against this via [`resolve_into`].
static mut CWD: [u8; 128] = [0; 128];
static mut CWD_LEN: usize = 0;

/// Called from `__libc_init`: seed the CWD to `/`.
pub fn vfs_init() {
    let c = unsafe { core::ptr::addr_of_mut!(CWD) };
    let l = unsafe { core::ptr::addr_of_mut!(CWD_LEN) };
    unsafe {
        (*c)[0] = b'/';
        (*c)[1] = 0;
        *l = 2;
    }
}

/// The CWD as a NUL-terminated byte slice.
pub fn get_cwd_nt() -> &'static [u8] {
    let c = unsafe { &*core::ptr::addr_of_mut!(CWD) };
    let l = unsafe { *core::ptr::addr_of_mut!(CWD_LEN) };
    &c[..l]
}

/// Resolve `path` against the CWD into `buf` (NUL-terminated). Returns the
/// resulting slice (including the trailing NUL) or `None` if it does not fit.
/// A trailing NUL on the input is tolerated.
pub fn resolve_into<'a>(path: &[u8], buf: &'a mut [u8]) -> Option<&'a [u8]> {
    let mut end = path.len();
    if end > 0 && path[end - 1] == 0 {
        end -= 1;
    }
    let path = &path[..end];
    if path.starts_with(b"/") {
        if path.len() + 1 > buf.len() {
            return None;
        }
        buf[..path.len()].copy_from_slice(path);
        buf[path.len()] = 0;
        Some(&buf[..path.len() + 1])
    } else {
        let cwd = get_cwd_nt();
        let clen = cwd.len().saturating_sub(1);
        let total = clen + path.len();
        if total + 1 > buf.len() {
            return None;
        }
        buf[..clen].copy_from_slice(&cwd[..clen]);
        buf[clen..total].copy_from_slice(path);
        buf[total] = 0;
        Some(&buf[..total + 1])
    }
}

/// Resolve a NUL-terminated C path into `buf`; returns the length (excluding
/// NUL) or `None`.
pub fn resolve_c(path: *const c_char, buf: &mut [u8]) -> Option<usize> {
    let p = unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) };
    let r = resolve_into(p, buf)?;
    Some(r.len() - 1)
}

/// POSIX `chdir(path)` — resolve against the current CWD, require a directory.
#[unsafe(no_mangle)]
pub extern "C" fn chdir(path: *const c_char) -> c_int {
    if path.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    let p = unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) };
    let mut rp = [0u8; 512];
    let Some(abs) = resolve_into(p, &mut rp) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    match stat_rs(abs) {
        Ok(s) if s.kind == 1 => {
            let c = unsafe { core::ptr::addr_of_mut!(CWD) };
            let l = unsafe { core::ptr::addr_of_mut!(CWD_LEN) };
            if abs.len() > 128 {
                errno::set(errno::ENAMETOOLONG);
                return -1;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(abs.as_ptr(), (*c).as_mut_ptr(), abs.len());
                *l = abs.len();
            }
            0
        }
        Ok(_) => {
            errno::set(errno::ENOTDIR);
            -1
        }
        Err(e) => {
            errno::set(e);
            -1
        }
    }
}

/// POSIX `getcwd(buf, size)` — absolute CWD as a NUL-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
    if buf.is_null() {
        errno::set(errno::EINVAL);
        return core::ptr::null_mut();
    }
    let cwd = get_cwd_nt();
    let clen = cwd.len();
    if clen > size {
        errno::set(errno::ERANGE);
        return core::ptr::null_mut();
    }
    unsafe {
        core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf as *mut u8, clen);
    }
    buf
}

/// `struct stat`-style snapshot from the VFS `:stat` method.
#[derive(Clone, Copy)]
pub struct Stat {
    pub ino: u64,
    pub size: u64,
    /// 0 = regular file, 1 = directory (matches `ObjectKind` tags).
    pub kind: u32,
    pub mtime: u64,
}

/// Split an absolute VFS path into `(parent, basename)` (both without
/// trailing slashes).  Returns `None` for a root path or a path with no
/// basename.  Paths are taken as `&[u8]` (the syscall needs NUL-terminated,
/// so callers pass the slice including the trailing NUL; we ignore it).
pub fn split_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    // Ignore any trailing NUL (callers pass NUL-terminated slices).
    let mut end = path.len();
    if end > 0 && path[end - 1] == 0 {
        end -= 1;
    }
    // Strip trailing slashes.
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let trimmed = &path[..end];
    let slash = trimmed.iter().rposition(|&c| c == b'/')?;
    let parent = &trimmed[..slash];
    let base = &trimmed[slash + 1..];
    if base.is_empty() {
        return None;
    }
    if parent.is_empty() {
        Some((b"/" as &[u8], base))
    } else {
        Some((parent, base))
    }
}

/// Build `obj:method` as a NUL-terminated path into `scratch`.  A trailing NUL
/// on `obj` is tolerated (callers pass `resolve_into`/`split_path` slices) so
/// the `:method` selector is never buried behind it — an embedded NUL would
/// make the kernel resolve the bare object and treat the write as a plain
/// value write instead of a method invocation.
fn method_path<'a>(obj: &[u8], method: &[u8], scratch: &'a mut [u8]) -> Option<&'a [u8]> {
    let mut olen = obj.len();
    if olen > 0 && obj[olen - 1] == 0 {
        olen -= 1;
    }
    let total = olen + 1 + method.len() + 1;
    if total > scratch.len() {
        return None;
    }
    scratch[..olen].copy_from_slice(&obj[..olen]);
    scratch[olen] = b':';
    scratch[olen + 1..olen + 1 + method.len()].copy_from_slice(method);
    scratch[olen + 1 + method.len()] = 0;
    Some(&scratch[..total])
}

/// Encode a `Str` value (`u32` LE length + bytes) into `buf`; returns the
/// used length.
fn enc_str<'a>(name: &[u8], buf: &'a mut [u8]) -> Option<&'a [u8]> {
    if name.len() > u32::MAX as usize || 4 + name.len() > buf.len() {
        return None;
    }
    buf[..4].copy_from_slice(&(name.len() as u32).to_le_bytes());
    buf[4..4 + name.len()].copy_from_slice(name);
    Some(&buf[..4 + name.len()])
}

/// Issue a single-`Str`-argument method (`create`/`mkdir`/`rmdir`/`unlink`)
/// on the directory containing `path`.
fn dir_method(path: &[u8], method: &[u8]) -> isize {
    let mut rp = [0u8; 512];
    let Some(rpath) = resolve_into(path, &mut rp) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let Some((parent, base)) = split_path(rpath) else {
        errno::set(errno::EINVAL);
        return -1;
    };
    let mut tp = [0u8; 512];
    let Some(mp) = method_path(parent, method, &mut tp) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let mut pay = [0u8; 260];
    let plen = {
        let Some(p) = enc_str(base, &mut pay) else {
            errno::set(errno::ENAMETOOLONG);
            return -1;
        };
        p.len()
    };
    errno::ret(unsafe { write_path(mp, &mut pay, plen, 0) })
}

/// POSIX `mkdir(path, mode)`.  `mode` is ignored (no permission model).
#[unsafe(no_mangle)]
pub extern "C" fn mkdir(path: *const core::ffi::c_char, _mode: core::ffi::c_uint) -> c_int {
    mkdir_rs(unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) })
}

/// Rust-friendly `mkdir` over a byte slice.
pub fn mkdir_rs(path: &[u8]) -> c_int {
    let r = dir_method(path, b"mkdir");
    if r < 0 { -1 } else { 0 }
}

/// Create a regular file via the parent directory's `:create` method.
pub fn create_rs(path: &[u8]) -> c_int {
    let r = dir_method(path, b"create");
    if r < 0 { -1 } else { 0 }
}

/// POSIX `rmdir(path)`.
#[unsafe(no_mangle)]
pub extern "C" fn rmdir(path: *const core::ffi::c_char) -> c_int {
    let r = dir_method(
        unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) },
        b"rmdir",
    );
    if r < 0 { -1 } else { 0 }
}

/// POSIX `unlink(path)`.
#[unsafe(no_mangle)]
pub extern "C" fn unlink(path: *const core::ffi::c_char) -> c_int {
    let r = dir_method(
        unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) },
        b"unlink",
    );
    if r < 0 { -1 } else { 0 }
}

/// POSIX `remove(path)` — unlink a file or remove an empty directory.
#[unsafe(no_mangle)]
pub extern "C" fn remove(path: *const c_char) -> c_int {
    let p = unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) };
    let r = dir_method(p, b"unlink");
    if r >= 0 {
        return 0;
    }
    // A directory must be removed with rmdir; retry.
    let r2 = dir_method(p, b"rmdir");
    if r2 >= 0 { 0 } else { -1 }
}

/// POSIX `rename(old, new)`.  Both names must live in the same directory
/// (the kernel's `:rename` is a single-directory method); a cross-directory
/// move returns ENOSYS.
#[unsafe(no_mangle)]
pub extern "C" fn rename(old: *const core::ffi::c_char, new: *const core::ffi::c_char) -> c_int {
    let o = unsafe { core::slice::from_raw_parts(old as *const u8, crate::string::strlen(old)) };
    let n = unsafe { core::slice::from_raw_parts(new as *const u8, crate::string::strlen(new)) };
    let mut ro = [0u8; 512];
    let mut rn = [0u8; 512];
    let Some(ro) = resolve_into(o, &mut ro) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let Some(rn) = resolve_into(n, &mut rn) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let Some((oparent, obase)) = split_path(ro) else {
        errno::set(errno::EINVAL);
        return -1;
    };
    let Some((nparent, nbase)) = split_path(rn) else {
        errno::set(errno::EINVAL);
        return -1;
    };
    if oparent != nparent {
        errno::set(errno::ENOSYS);
        return -1;
    }
    let mut tp = [0u8; 512];
    let Some(mp) = method_path(oparent, b"rename", &mut tp) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    // Payload: Struct[Str old, Str new].
    let mut pay = [0u8; 520];
    let (alen, blen) = {
        let Some(a) = enc_str(obase, &mut pay) else {
            errno::set(errno::ENAMETOOLONG);
            return -1;
        };
        let alen = a.len();
        let Some(b) = enc_str(nbase, &mut pay[alen..]) else {
            errno::set(errno::ENAMETOOLONG);
            return -1;
        };
        (alen, b.len())
    };
    let total = alen + blen;
    let r = errno::ret(unsafe { write_path(mp, &mut pay, total, 0) });
    if r < 0 { -1 } else { 0 }
}

/// POSIX `truncate(path, length)`.
#[unsafe(no_mangle)]
pub extern "C" fn truncate(path: *const core::ffi::c_char, length: core::ffi::c_longlong) -> c_int {
    truncate_rs(
        unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) },
        length.max(0) as u64,
    )
}

/// Rust-friendly `truncate` over a byte-slice path.
pub fn truncate_rs(path: &[u8], len: u64) -> c_int {
    let mut rp = [0u8; 512];
    let Some(rpath) = resolve_into(path, &mut rp) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let mut tp = [0u8; 512];
    let Some(mp) = method_path(rpath, b"truncate", &mut tp) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let mut pay = [0u8; 8];
    pay[..8].copy_from_slice(&len.to_le_bytes());
    let r = errno::ret(unsafe { write_path(mp, &mut pay, 8, 0) });
    if r < 0 { -1 } else { 0 }
}

/// Query a path's stat snapshot.  Returns `Ok(stat)` or the errno.
pub fn stat_rs(path: &[u8]) -> Result<Stat, c_int> {
    let mut rp = [0u8; 512];
    let Some(rpath) = resolve_into(path, &mut rp) else {
        return Err(errno::ENAMETOOLONG);
    };
    let mut tp = [0u8; 512];
    let Some(mp) = method_path(rpath, b"stat", &mut tp) else {
        return Err(errno::ENAMETOOLONG);
    };
    let mut buf = [0u8; 32];
    let r = unsafe { write_path(mp, &mut buf, 32, 0) };
    if r < 28 {
        return Err(errno::ret(r) as c_int);
    }
    let le = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    Ok(Stat {
        ino: le(0),
        size: le(8),
        kind: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
        mtime: le(20),
    })
}

/// POSIX `stat(path, *buf)` where `buf` points to the C `struct stat`
/// declared in `<sys/stat.h>` (32-byte layout: ino, size, mode, mtime).
/// The VFS `kind` tag (0 = file, 1 = directory) is translated into
/// `S_IFREG` / `S_IFDIR` mode bits.
#[unsafe(no_mangle)]
pub extern "C" fn stat(path: *const core::ffi::c_char, buf: *mut u8) -> c_int {
    let p = unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) };
    match stat_rs(p) {
        Ok(s) => {
            unsafe {
                core::ptr::write_bytes(buf, 0, 32);
                *(buf as *mut u64) = s.ino;
                *((buf as *mut u64).add(1)) = s.size;
                *((buf as *mut u32).add(4)) = if s.kind == 1 { 0o040000 } else { 0o100000 };
                *((buf as *mut u64).add(3)) = s.mtime;
            }
            0
        }
        Err(e) => {
            errno::set(e);
            -1
        }
    }
}

/// POSIX `lstat` — same as `stat` (no symlink support in the VFS).
#[unsafe(no_mangle)]
pub extern "C" fn lstat(path: *const core::ffi::c_char, buf: *mut u8) -> c_int {
    stat(path, buf)
}

/// Rust `access`-style check: `F_OK` tests existence via `:stat`.
pub fn exists_rs(path: &[u8]) -> bool {
    stat_rs(path).is_ok()
}
