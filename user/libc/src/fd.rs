//! POSIX file-descriptor layer — pure userspace shim over Unispace paths.
//!
//! fds 0/1/2 are the task's standard streams (`/proc/self/std/{in,out,err}`).
//! fds 3..N are path-backed handles opened via `open()`, tracked in a fixed
//! `.bss` pool (single-threaded tasks). Sharing (`dup`/`dup2`) is via a
//! reference-counted open-file description (Desc) so `dup` shares offset and
//! file-status flags (APPEND/SYNC) as POSIX requires. The kernel has no fd
//! table; every read/write is a positioned `read_path(path,buf,offset)` /
//! `write_path(path,buf,flags)` (offset in `arg4`: `0x1` = APPEND,
//! otherwise `offset<<8`).

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

// ── pools ─────────────────────────────────────────────────────────────

const FD_POOL: usize = 32;
const DESC_POOL: usize = 32;
const PATH_CAP: usize = 128;

#[derive(Clone, Copy)]
struct Desc {
    path: [u8; PATH_CAP],
    plen: usize,
    offset: u64,
    readable: bool,
    writable: bool,
    append: bool,
    sync: bool,
    refcnt: usize,
}

const DESC_INIT: Desc = Desc {
    path: [0; PATH_CAP],
    plen: 0,
    offset: 0,
    readable: false,
    writable: false,
    append: false,
    sync: false,
    refcnt: 0,
};

static mut DESCS: [Desc; DESC_POOL] = [DESC_INIT; DESC_POOL];

#[derive(Clone, Copy)]
struct FdEntry {
    desc: Option<u8>,
    cloexec: bool,
    used: bool,
}

const FD_INIT: FdEntry = FdEntry {
    desc: None,
    cloexec: false,
    used: false,
};

static mut FDS: [FdEntry; FD_POOL] = [FD_INIT; FD_POOL];

fn fd_ref(fd: c_int) -> Option<&'static mut FdEntry> {
    if fd < 3 || fd as usize >= FD_POOL {
        return None;
    }
    let p = core::ptr::addr_of_mut!(FDS) as *mut FdEntry;
    Some(unsafe { &mut *p.add(fd as usize) })
}

fn desc_ref(idx: u8) -> &'static mut Desc {
    let p = core::ptr::addr_of_mut!(DESCS) as *mut Desc;
    unsafe { &mut *p.add(idx as usize) }
}

fn alloc_desc(path: &[u8], readable: bool, writable: bool, append: bool, sync: bool) -> Option<u8> {
    let base = core::ptr::addr_of_mut!(DESCS) as *mut Desc;
    for i in 0..DESC_POOL {
        let d = unsafe { &mut *base.add(i) };
        if d.refcnt == 0 {
            d.path = [0; PATH_CAP];
            let n = core::cmp::min(path.len(), PATH_CAP - 1);
            d.path[..n].copy_from_slice(&path[..n]);
            d.path[n] = 0;
            d.plen = n;
            d.offset = 0;
            d.readable = readable;
            d.writable = writable;
            d.append = append;
            d.sync = sync;
            d.refcnt = 1;
            return Some(i as u8);
        }
    }
    None
}

fn alloc_fd(desc_idx: u8) -> c_int {
    let clo = desc_ref(desc_idx).sync; // not cloexec; will be set via O_CLOEXEC
    let _ = clo;
    for i in 3..FD_POOL {
        let p = core::ptr::addr_of_mut!(FDS) as *mut FdEntry;
        let f = unsafe { &mut *p.add(i) };
        if !f.used {
            f.desc = Some(desc_idx);
            f.cloexec = false;
            f.used = true;
            return i as c_int;
        }
    }
    errno::set(errno::EMFILE);
    -1
}

// symlink test via readlink (no-follow)
fn is_symlink(path: &[u8]) -> bool {
    // Need NUL-terminated copy
    let mut tmp = [0u8; 256];
    if path.len() + 1 > tmp.len() {
        return false;
    }
    tmp[..path.len()].copy_from_slice(path);
    tmp[path.len()] = 0;
    let mut out = [0u8; 256];
    // Use libc readlink which does parent:readlink (no follow)
    let r = unsafe {
        crate::vfs::readlink(tmp.as_ptr() as *const c_char, out.as_mut_ptr() as *mut c_char, out.len())
    };
    r >= 0
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
    let sync = flags & O_SYNC != 0;
    let trunc = flags & O_TRUNC != 0;
    let creat = flags & O_CREAT != 0;
    let excl = flags & O_EXCL != 0;
    let nofollow = flags & O_NOFOLLOW != 0;
    let directory = flags & O_DIRECTORY != 0;

    // O_NOFOLLOW — fail if final is symlink
    if nofollow && is_symlink(p) {
        errno::set(errno::ELOOP);
        return -1;
    }

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
    if trunc && writable {
        // For directories truncate is meaningless; it will fail via vfs and we propagate.
        if crate::vfs::truncate_rs(p, 0) < 0 {
            return -1;
        }
    }

    // O_DIRECTORY — require target be a directory (after create/trunc)
    if directory {
        match crate::vfs::stat_rs(p) {
            Ok(s) if s.kind == 1 => {}
            Ok(_) => {
                errno::set(errno::ENOTDIR);
                return -1;
            }
            Err(e) => {
                errno::set(e);
                return -1;
            }
        }
    }

    let desc_idx = match alloc_desc(p, readable, writable, append, sync) {
        Some(v) => v,
        None => {
            errno::set(errno::EMFILE);
            return -1;
        }
    };
    let fd = alloc_fd(desc_idx);
    if fd < 0 {
        // rollback desc
        let d = desc_ref(desc_idx);
        d.refcnt = 0;
        return -1;
    }
    // O_CLOEXEC — per-fd flag
    if flags & O_CLOEXEC != 0 {
        if let Some(fe) = fd_ref(fd) {
            fe.cloexec = true;
        }
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
        return 0;
    }
    let Some(fe) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !fe.used {
        errno::set(errno::EBADF);
        return -1;
    }
    if let Some(idx) = fe.desc {
        let d = desc_ref(idx);
        if d.refcnt > 0 {
            d.refcnt -= 1;
            if d.refcnt == 0 {
                // clear for reuse
                d.plen = 0;
            }
        }
    }
    fe.desc = None;
    fe.used = false;
    fe.cloexec = false;
    0
}

/// POSIX `dup(fd)` — duplicate into the lowest free fd, sharing offset.
#[unsafe(no_mangle)]
pub extern "C" fn dup(fd: c_int) -> c_int {
    if fd < 3 {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(fe) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !fe.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(idx) = fe.desc else {
        errno::set(errno::EBADF);
        return -1;
    };
    // inc ref
    desc_ref(idx).refcnt += 1;
    let new_fd = alloc_fd(idx);
    if new_fd < 0 {
        // rollback
        let d = desc_ref(idx);
        if d.refcnt > 0 { d.refcnt -= 1; }
        return -1;
    }
    new_fd
}

/// POSIX `dup2(old, new)` — force `new` to be a copy of `old`.
#[unsafe(no_mangle)]
pub extern "C" fn dup2(old: c_int, new: c_int) -> c_int {
    if old == new {
        // verify old valid
        if old < 3 {
            errno::set(errno::EBADF);
            return -1;
        }
        let Some(fe) = fd_ref(old) else {
            errno::set(errno::EBADF);
            return -1;
        };
        if !fe.used {
            errno::set(errno::EBADF);
            return -1;
        }
        return new;
    }
    if old < 3 || new < 3 {
        errno::set(errno::EBADF);
        return -1;
    }
    // validate new range
    if new as usize >= FD_POOL {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(ofe) = fd_ref(old) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !ofe.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(idx) = ofe.desc else {
        errno::set(errno::EBADF);
        return -1;
    };
    // close new if open
    {
        if let Some(nf) = fd_ref(new) {
            if nf.used {
                // close logic inline
                if let Some(nidx) = nf.desc {
                    let d = desc_ref(nidx);
                    if d.refcnt > 0 {
                        d.refcnt -= 1;
                        if d.refcnt == 0 { d.plen = 0; }
                    }
                }
                nf.desc = None;
                nf.used = false;
                nf.cloexec = false;
            }
        }
    }
    desc_ref(idx).refcnt += 1;
    let nf = fd_ref(new).unwrap();
    nf.desc = Some(idx);
    nf.used = true;
    nf.cloexec = false;
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
            errno::set(errno::EBADF);
            -1
        }
        _ => {
            let Some(fe) = fd_ref(fd) else {
                errno::set(errno::EBADF);
                return -1;
            };
            if !fe.used {
                errno::set(errno::EBADF);
                return -1;
            }
            let Some(idx) = fe.desc else {
                errno::set(errno::EBADF);
                return -1;
            };
            let d = desc_ref(idx);
            if !d.readable {
                errno::set(errno::EBADF);
                return -1;
            }
            let off = d.offset;
            let path = {
                let mut tmp = [0u8; 129];
                tmp[..d.plen].copy_from_slice(&d.path[..d.plen]);
                tmp[d.plen] = 0;
                tmp
            };
            // need to copy path slice for read_path
            let p = &path[..d.plen + 1];
            let r = unsafe { read_path(p, b, off) };
            if r >= 0 {
                d.offset = off.saturating_add(r as u64);
            }
            errno::ret(r)
        }
    }
}

/// Raw `write(fd, buf, len)`.  The kernel consumes the buffer in place, so
/// file writes go through chunked scratch.
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
            let Some(fe) = fd_ref(fd) else {
                errno::set(errno::EBADF);
                return -1;
            };
            if !fe.used {
                errno::set(errno::EBADF);
                return -1;
            }
            let Some(idx) = fe.desc else {
                errno::set(errno::EBADF);
                return -1;
            };
            let (writable, append, base, path_copy, plen) = {
                let d = desc_ref(idx);
                if !d.writable {
                    errno::set(errno::EBADF);
                    return -1;
                }
                let mut tmp = [0u8; 129];
                tmp[..d.plen].copy_from_slice(&d.path[..d.plen]);
                tmp[d.plen] = 0;
                (d.writable, d.append, d.offset, tmp, d.plen)
            };
            let _ = writable;
            let data = data_slice(buf, len);
            let r = if append {
                chunked_write_append(&path_copy[..plen + 1], data)
            } else {
                chunked_write_positioned(&path_copy[..plen + 1], data, base)
            };
            if r >= 0 && !append {
                let d = desc_ref(idx);
                d.offset = base.saturating_add(r as u64);
            }
            // O_SYNC — no extra kernel flag; durability is via periodic sync.
            // The SYNC bit is stored per-desc and visible via fcntl.
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

/// Append write: each chunk appends at EOF (0x1).
fn chunked_write_append(path: &[u8], data: &[u8]) -> isize {
    if data.is_empty() {
        return 0;
    }
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
        let r = unsafe { write_path(path, scratch, n, 0x1) };
        if r < 0 {
            return r;
        }
        off += n;
    }
    data.len() as isize
}

/// Positioned write: each chunk's flag is (base+off)<<8.
fn chunked_write_positioned(path: &[u8], data: &[u8], base: u64) -> isize {
    if data.is_empty() {
        return 0;
    }
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
        let flags = (base + off as u64) << 8;
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
        errno::set(errno::ESPIPE);
        return -1;
    }
    let Some(fe) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !fe.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(idx) = fe.desc else {
        errno::set(errno::EBADF);
        return -1;
    };
    let d = desc_ref(idx);
    let base: i64 = match whence {
        0 => 0,
        1 => d.offset as i64,
        2 => {
            let mut tmp = [0u8; 129];
            tmp[..d.plen].copy_from_slice(&d.path[..d.plen]);
            tmp[d.plen] = 0;
            // stat needs NUL-terminated? stat_rs takes &[u8] without NUL
            match crate::vfs::stat_rs(&d.path[..d.plen]) {
                Ok(s) => s.size as i64,
                Err(e) => {
                    errno::set(e);
                    return -1;
                }
            }
        }
        _ => {
            errno::set(errno::EINVAL);
            return -1;
        }
    };
    let new = base.saturating_add(offset as i64);
    if new < 0 {
        errno::set(errno::EINVAL);
        return -1;
    }
    d.offset = new as u64;
    new as c_long
}

/// POSIX `fstat(fd, *buf)` — fills the C `struct stat` layout (ino, size,
/// mode, mtime) exactly like `stat()`.
#[unsafe(no_mangle)]
pub extern "C" fn fstat(fd: c_int, buf: *mut u8) -> c_int {
    if fd < 3 {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(fe) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !fe.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(idx) = fe.desc else {
        errno::set(errno::EBADF);
        return -1;
    };
    let d = desc_ref(idx);
    match crate::vfs::stat_rs(&d.path[..d.plen]) {
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
    let Some(fe) = fd_ref(fd) else {
        errno::set(errno::EBADF);
        return -1;
    };
    if !fe.used {
        errno::set(errno::EBADF);
        return -1;
    }
    let Some(idx) = fe.desc else {
        errno::set(errno::EBADF);
        return -1;
    };
    let d = desc_ref(idx);
    if !d.writable {
        errno::set(errno::EBADF);
        return -1;
    }
    if length < 0 {
        errno::set(errno::EINVAL);
        return -1;
    }
    crate::vfs::truncate_rs(&d.path[..d.plen], length as u64)
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
    let fe = match fd_ref(fd) {
        Some(f) if f.used => f,
        _ => {
            errno::set(errno::EBADF);
            return -1;
        }
    };
    let Some(idx) = fe.desc else {
        errno::set(errno::EBADF);
        return -1;
    };
    match cmd {
        0 => {
            // F_DUPFD — duplicate to lowest fd >= arg.
            if arg < 3 { return dup(fd); }
            // Implement arg hint: find lowest free >= arg
            for i in (arg as usize)..FD_POOL {
                let p = core::ptr::addr_of_mut!(FDS) as *mut FdEntry;
                let cand = unsafe { &mut *p.add(i) };
                if !cand.used {
                    desc_ref(idx).refcnt += 1;
                    cand.desc = Some(idx);
                    cand.used = true;
                    cand.cloexec = false;
                    return i as c_int;
                }
            }
            errno::set(errno::EMFILE);
            -1
        }
        1 => {
            // F_GETFD
            if fe.cloexec { 1 } else { 0 }
        }
        2 => {
            // F_SETFD
            fe.cloexec = (arg & 1) != 0;
            0
        }
        3 => {
            // F_GETFL
            let d = desc_ref(idx);
            let mut fl = if d.readable && d.writable {
                O_RDWR
            } else if d.writable {
                O_WRONLY
            } else {
                O_RDONLY
            };
            if d.append {
                fl |= O_APPEND;
            }
            if d.sync {
                fl |= O_SYNC;
            }
            fl
        }
        4 => {
            // F_SETFL — O_APPEND and O_SYNC are the only meaningful bits.
            let d = desc_ref(idx);
            d.append = arg & O_APPEND != 0;
            d.sync = arg & O_SYNC != 0;
            0
        }
        5 | 6 | 7 => {
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
    let Some(fe) = fd_ref(fd) else { return false };
    if !fe.used { return false }
    let Some(idx) = fe.desc else { return false };
    let d = desc_ref(idx);
    if d.plen != file_path.len() { return false }
    d.path[..d.plen] == file_path[..]
}

/// Helper for `fdopen(int, mode)`: retrieve the NUL-terminated path for `fd`.
pub fn fd_path(fd: c_int) -> Option<[u8; 128]> {
    if fd < 0 || fd as usize >= FD_POOL {
        return None;
    }
    if fd <= 2 {
        return None;
    }
    let fe = fd_ref(fd)?;
    if !fe.used { return None }
    let Some(idx) = fe.desc else { return None };
    let d = desc_ref(idx);
    let mut arr = [0u8; 128];
    arr[..d.plen].copy_from_slice(&d.path[..d.plen]);
    arr[d.plen] = 0;
    Some(arr)
}
