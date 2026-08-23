//! POSIX file-descriptor layer.
//!
//! fds 0/1/2 are the task's standard streams (`/proc/self/std/{in,out,err}`).
//! fds 3..N are path-backed handles opened via `open()`, tracked in a fixed
//! `.bss` pool (single-threaded tasks, matching the rest of the crate).  The
//! kernel's VFS read/write contract is positioned: `read` takes a byte offset
//! in `arg4`, `write` takes an APPEND bit or a WRITE-AT offset.

use core::ffi::{c_char, c_int, c_long, c_uint, c_void};

use crate::errno;
use crate::syscall::{read_path, write_path};

// ── open(2) flags (Linux x86_64 numbers) ──────────────────────────────

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_ACCMODE: c_int = 3;
pub const O_CREAT: c_int = 0o100;
pub const O_EXCL: c_int = 0o200;
pub const O_TRUNC: c_int = 0o1000;
pub const O_APPEND: c_int = 0o2000;
pub const O_NONBLOCK: c_int = 0o4000;
pub const O_CLOEXEC: c_int = 0o2000000;
pub const O_DIRECTORY: c_int = 0o200000;
pub const O_NOFOLLOW: c_int = 0o400000;
pub const O_SYNC: c_int = 0o10000;

// ── fd table ──────────────────────────────────────────────────────────

const FD_POOL: usize = 32;
const PATH_CAP: usize = 128;

#[derive(Clone, Copy)]
struct Fd {
    path: [u8; PATH_CAP],
    plen: usize,
    offset: u64,
    readable: bool,
    writable: bool,
    append: bool,
    used: bool,
}

const FD_INIT: Fd = Fd {
    path: [0; PATH_CAP],
    plen: 0,
    offset: 0,
    readable: false,
    writable: false,
    append: false,
    used: false,
};

static mut FDS: [Fd; FD_POOL] = [FD_INIT; FD_POOL];

fn fd_ref(fd: c_int) -> Option<&'static mut Fd> {
    if fd < 3 || fd as usize >= FD_POOL {
        return None;
    }
    let p = core::ptr::addr_of_mut!(FDS) as *mut Fd;
    Some(unsafe { &mut *p.add(fd as usize) })
}

/// NUL-terminated path slice for an fd.
fn alloc_fd(path: &[u8], readable: bool, writable: bool, append: bool) -> c_int {
    for i in 3..FD_POOL {
        let p = core::ptr::addr_of_mut!(FDS) as *mut Fd;
        let f = unsafe { &mut *p.add(i) };
        if !f.used {
            f.path = [0; PATH_CAP];
            let n = core::cmp::min(path.len(), PATH_CAP - 1);
            f.path[..n].copy_from_slice(&path[..n]);
            f.path[n] = 0;
            f.plen = n;
            f.offset = 0;
            f.readable = readable;
            f.writable = writable;
            f.append = append;
            f.used = true;
            return i as c_int;
        }
    }
    errno::set(errno::EMFILE);
    -1
}

// ── open(2) ───────────────────────────────────────────────────────────

/// POSIX `open(path, flags, ...)`.  `mode` is ignored (no permission model).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    let _ = mode;
    let mut buf = [0u8; PATH_CAP];
    let Some(plen) = crate::vfs::resolve_c(path, &mut buf) else {
        errno::set(errno::ENAMETOOLONG);
        return -1;
    };
    let p = &buf[..plen];

    let acc = flags & O_ACCMODE;
    let writable = acc != O_RDONLY;
    let readable = acc != O_WRONLY;
    let append = flags & O_APPEND != 0;
    let trunc = flags & O_TRUNC != 0;
    let creat = flags & O_CREAT != 0;
    let excl = flags & O_EXCL != 0;

    let exists = crate::vfs::stat_rs(p).is_ok();
    if creat && !exists {
        if crate::vfs::create_rs(p) < 0 {
            return -1;
        }
    } else if creat && exists && excl {
        errno::set(errno::EEXIST);
        return -1;
    } else if !creat && !exists {
        errno::set(errno::ENOENT);
        return -1;
    }
    if trunc && writable && crate::vfs::truncate_rs(p, 0) < 0 {
        return -1;
    }

    let fd = alloc_fd(p, readable, writable, append);
    if fd < 0 {
        return -1;
    }
    fd
}

/// POSIX `creat(path, mode)` — open write-only with O_CREAT|O_TRUNC.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn creat(path: *const c_char, mode: c_uint) -> c_int {
    unsafe { open(path, O_WRONLY | O_CREAT | O_TRUNC, mode) }
}

// ── close / dup ───────────────────────────────────────────────────────

/// POSIX `close(fd)`.
#[unsafe(no_mangle)]
pub extern "C" fn close(fd: c_int) -> c_int {
    if fd < 3 {
        // Standard streams have no table entry; closing them is a no-op.
        return 0;
    }
    match fd_ref(fd) {
        Some(f) => {
            f.used = false;
            0
        }
        None => {
            errno::set(errno::EBADF);
            -1
        }
    }
}

/// POSIX `dup(fd)` — duplicate into the lowest free fd.
#[unsafe(no_mangle)]
pub extern "C" fn dup(fd: c_int) -> c_int {
    if fd < 3 {
        // Can't faithfully duplicate a kernel-owned stream; reject.
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(f) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !f.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let copy = *f;
    alloc_fd(&f.path[..f.plen], copy.readable, copy.writable, copy.append)
}

/// POSIX `dup2(old, new)` — force `new` to be a copy of `old`.
#[unsafe(no_mangle)]
pub extern "C" fn dup2(old: c_int, new: c_int) -> c_int {
    if old == new {
        return new;
    }
    if old < 3 || new < 3 {
        // Refuse to remap the kernel-owned std streams.
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(f) = fd_ref(old) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !f.used {
        errno::set(errno::EBADF);
        return -1;
    }
    if let Some(nf) = fd_ref(new) {
        nf.used = false;
    }
    let copy = *f;
    let new_fd = alloc_fd(&f.path[..f.plen], copy.readable, copy.writable, copy.append);
    if new_fd != new {
        // alloc_fd picks the lowest free slot; force it onto `new`.
        if let Some(nf) = fd_ref(new) {
            *nf = copy;
            nf.used = true;
        }
        if new_fd >= 0 {
            if let Some(of) = fd_ref(new_fd) {
                of.used = false;
            }
        }
    }
    new
}

// ── read / write ──────────────────────────────────────────────────────

/// Raw `read(fd, buf, len)`.  Handles the std streams and path fds.
pub fn read_fd(fd: c_int, buf: *mut c_void, len: usize) -> isize {
    if fd < 0 {
        errno::set(errno::EBADF);
        return -1;
    }
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    let b = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    match fd {
        0 => {
            let r = unsafe { read_path(b"/proc/self/std/in\0", b, 0) };
            errno::ret(r)
        }
        1 | 2 => {
            // Writing to a read-only stream via read makes no sense.
            errno::set(errno::EBADF);
            -1
        }
        _ => {
            let Some(f) = fd_ref(fd) else {
                errno::set(errno::EBADF);
                return -1;
            };
            if !f.used || !f.readable {
                errno::set(errno::EBADF);
                return -1;
            }
            let off = f.offset;
            let path = &f.path[..f.plen + 1];
            let r = unsafe { read_path(path, b, off) };
            if r >= 0 {
                f.offset = off.saturating_add(r as u64);
            }
            errno::ret(r)
        }
    }
}

/// Raw `write(fd, buf, len)`.  The kernel consumes the buffer in place, so
/// file writes go through `write_data`'s chunked scratch path.
pub fn write_fd(fd: c_int, buf: *const c_void, len: usize) -> isize {
    if fd < 0 {
        errno::set(errno::EBADF);
        return -1;
    }
    if len > 0 && buf.is_null() {
        errno::set(errno::EFAULT);
        return -1;
    }
    match fd {
        1 => {
            let r = crate::syscall::write_data(b"/proc/self/std/out\0", data_slice(buf, len), 0);
            errno::ret(r)
        }
        2 => {
            let r = crate::syscall::write_data(b"/proc/self/std/err\0", data_slice(buf, len), 0);
            errno::ret(r)
        }
        0 => {
            errno::set(errno::EBADF);
            -1
        }
        _ => {
            let Some(f) = fd_ref(fd) else {
                errno::set(errno::EBADF);
                return -1;
            };
            if !f.used || !f.writable {
                errno::set(errno::EBADF);
                return -1;
            }
            let base = f.offset;
            let path = &f.path[..f.plen + 1];
            let flags = if f.append { 0x1 } else { base << 8 };
            let data = data_slice(buf, len);
            let r = chunked_write(path, data, flags);
            if r >= 0 && !f.append {
                f.offset = base.saturating_add(r as u64);
            }
            errno::ret(r)
        }
    }
}

fn data_slice<'a>(buf: *const c_void, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(buf as *const u8, len) }
    }
}

/// Chunked positioned/append write through static scratch (the write syscall
/// consumes its buffer in place, so the caller's data must be copied).
fn chunked_write(path: &[u8], data: &[u8], flags: u64) -> isize {
    static mut SCRATCH: [u8; crate::IO_CHUNK_BYTES] = [0; crate::IO_CHUNK_BYTES];
    let scratch = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(SCRATCH) as *mut u8,
            crate::IO_CHUNK_BYTES,
        )
    };
    let mut off = 0usize;
    while off < data.len() {
        let n = core::cmp::min(data.len() - off, crate::IO_CHUNK_BYTES);
        scratch[..n].copy_from_slice(&data[off..off + n]);
        let r = unsafe { write_path(path, scratch, n, flags) };
        if r < 0 {
            return r;
        }
        off += n;
    }
    data.len() as isize
}

// ── positioning ───────────────────────────────────────────────────────

/// POSIX `lseek(fd, offset, whence)`.  SEEK_SET=0, SEEK_CUR=1, SEEK_END=2.
#[unsafe(no_mangle)]
pub extern "C" fn lseek(fd: c_int, offset: c_long, whence: c_int) -> c_long {
    if fd < 3 {
        // Standard streams are not seekable.
        errno::set(errno::ESPIPE);
        return -1;
    }
    let Some(f) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !f.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let base: i64 = match whence {
        0 => 0,
        1 => f.offset as i64,
        2 => match crate::vfs::stat_rs(&f.path[..f.plen]) {
            Ok(s) => s.size as i64,
            Err(e) => {
                errno::set(e);
                return -1;
            }
        },
        _ => {
            errno::set(errno::EINVAL);
            return -1;
        }
    };
    let new = base.saturating_add(offset);
    if new < 0 {
        errno::set(errno::EINVAL);
        return -1;
    }
    f.offset = new as u64;
    new
}

/// POSIX `fstat(fd, *buf)` — fills the C `struct stat` layout (ino, size,
/// mode, mtime) exactly like `stat()`.
#[unsafe(no_mangle)]
pub extern "C" fn fstat(fd: c_int, buf: *mut u8) -> c_int {
    if fd < 3 {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(f) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !f.used {
        errno::set(errno::EBADF);
        return -1;
    }
    match crate::vfs::stat_rs(&f.path[..f.plen]) {
        Ok(s) => {
            let base = match s.kind {
                1 => 0o040000,
                2 => 0o120000,
                3 => 0o010000,
                4 => 0o020000,
                5 => 0o060000,
                6 => 0o140000,
                _ => 0o100000,
            };
            let mode = base | (s.mode & 0o7777);
            unsafe {
                core::ptr::write_bytes(buf, 0, 32);
                *(buf as *mut u64) = s.ino;
                *((buf as *mut u64).add(1)) = s.size;
                *((buf as *mut u32).add(4)) = mode;
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

/// POSIX `ftruncate(fd, length)`.
#[unsafe(no_mangle)]
pub extern "C" fn ftruncate(fd: c_int, length: c_long) -> c_int {
    if fd < 3 {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(f) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !f.used || !f.writable {
        errno::set(errno::EBADF);
        return -1;
    }
    if length < 0 {
        errno::set(errno::EINVAL);
        return -1;
    }
    crate::vfs::truncate_rs(&f.path[..f.plen], length as u64)
}

/// `openat(dirfd, path, flags, ...)` — dirfd ignored except AT_FDCWD (-100); otherwise same as `open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    let _ = dirfd;
    unsafe { open(path, flags, mode) }
}

#[unsafe(no_mangle)]
pub extern "C" fn posix_fadvise(_fd: c_int, _offset: c_long, _len: c_long, _advice: c_int) -> c_int {
    0
}

/// Minimal `fcntl`: supports F_DUPFD/F_GETFD/F_SETFD/F_GETFL/F_SETFL.
#[unsafe(no_mangle)]
pub extern "C" fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int {
    let f = match fd_ref(fd) {
        Some(f) => f,
        None => {
            errno::set(errno::EBADF);
            return -1;
        }
    };
    match cmd {
        0 => {
            // F_DUPFD — duplicate to lowest fd >= arg.
            if arg < 3 { return dup(fd); }
            // Simplistic: ignore `arg` hint beyond checking free.
            dup(fd)
        }
        1 => {
            // F_GETFD — no CLOEXEC tracking; return 0.
            0
        }
        2 => {
            // F_SETFD — ignore FD_CLOEXEC.
            0
        }
        3 => {
            // F_GETFL
            let mut fl = if f.readable && f.writable {
                O_RDWR
            } else if f.writable {
                O_WRONLY
            } else {
                O_RDONLY
            };
            if f.append {
                fl |= O_APPEND;
            }
            fl
        }
        4 => {
            // F_SETFL — only O_APPEND is meaningful here.
            f.append = arg & O_APPEND != 0;
            0
        }
        5 | 6 | 7 => {
            // F_GETLK / F_SETLK — no locks; succeed.
            0
        }
        _ => {
            errno::set(errno::ENOSYS);
            -1
        }
    }
}

/// Helper for `fileno(FILE*)`: true when `fd`'s path equals `file_path`.
pub fn fileno_path(fd: c_int, file_path: &[u8]) -> bool {
    let Some(f) = fd_ref(fd) else { return false };
    if !f.used { return false }
    if f.plen != file_path.len() { return false }
    f.path[..f.plen] == file_path[..]
}

/// Helper for `fdopen(int, mode)`: retrieve the NUL-terminated path for `fd`.
pub fn fd_path(fd: c_int) -> Option<[u8; 128]> {
    if fd < 0 || fd as usize >= FD_POOL {
        return None;
    }
    if fd <= 2 {
        return None; // std fds are not in table but caller handles them separately
    }
    let f = fd_ref(fd)?;
    if !f.used { return None }
    let mut arr = [0u8; 128];
    arr[..f.plen].copy_from_slice(&f.path[..f.plen]);
    arr[f.plen] = 0;
    Some(arr)
}
