//! `<dirent.h>` — directory streams over the kernel's VFS listing wire.
//!
//! `read(dir_path)` returns the encoded `DIR_SCHEMA` listing:
//! `Struct[ List[ Struct[ Str name, Enum kind ] ] ]`, i.e. `u32` count
//! followed by `(u32 namelen, name, u32 kind)` per entry.  We snapshot the
//! listing once in `opendir` and walk it from a `.bss` pool (single-threaded
//! tasks).

use core::ffi::{c_char, c_int, c_void};

use crate::errno;
use crate::syscall::read_path;

// POSIX d_type values.
pub const DT_UNKNOWN: u8 = 0;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;

/// `struct dirent` — name plus a small d_type tag.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dirent {
    pub d_ino: u64,
    pub d_type: u8,
    pub d_name: [c_char; 256],
}

impl Dirent {
    const INIT: Dirent = Dirent {
        d_ino: 0,
        d_type: DT_UNKNOWN,
        d_name: [0; 256],
    };
}

/// Opaque directory-stream handle.
#[repr(C)]
pub struct DIR {
    path: [u8; 128],
    plen: usize,
    snap: [u8; 8192],
    snap_len: usize,
    pos: usize,
    count: usize,
    seen: usize,
    cur: Dirent,
}

const DIR_INIT: DIR = DIR {
    path: [0; 128],
    plen: 0,
    snap: [0; 8192],
    snap_len: 0,
    pos: 0,
    count: 0,
    seen: 0,
    cur: Dirent::INIT,
};

const DIR_POOL: usize = 8;

static mut DIRS: [DIR; DIR_POOL] = [DIR_INIT; DIR_POOL];

fn dir_slot() -> Option<&'static mut DIR> {
    for i in 0..DIR_POOL {
        let p = unsafe { core::ptr::addr_of_mut!(DIRS) } as *mut DIR;
        let d = unsafe { &mut *p.add(i) };
        // A slot is free when its snap_len is 0 (no active snapshot).
        if d.snap_len == 0 {
            return Some(d);
        }
    }
    None
}

fn is_open(d: &DIR) -> bool {
    d.snap_len != 0
}

/// POSIX `opendir(path)` — snapshot the directory listing.
#[unsafe(no_mangle)]
pub extern "C" fn opendir(path: *const c_char) -> *mut DIR {
    if path.is_null() {
        errno::set(errno::EINVAL);
        return core::ptr::null_mut();
    }
    let plen = crate::string::strlen(path);
    if plen == 0 || plen >= 128 {
        errno::set(errno::ENAMETOOLONG);
        return core::ptr::null_mut();
    }
    let d = match dir_slot() {
        Some(d) => d,
        None => {
            errno::set(errno::EMFILE);
            return core::ptr::null_mut();
        }
    };
    unsafe {
        core::ptr::copy_nonoverlapping(path as *const u8, d.path.as_mut_ptr(), plen);
        d.path[plen] = 0;
    }
    d.plen = plen;
    d.snap_len = 0;
    d.pos = 0;
    d.count = 0;
    d.seen = 0;
    let mut snap_len = d.snap.len();
    let r = unsafe { read_path(&d.path[..plen + 1], &mut d.snap, 0) };
    if r < 0 {
        errno::set((-r) as c_int);
        d.snap_len = 0;
        return core::ptr::null_mut();
    }
    snap_len = r as usize;
    d.snap_len = snap_len;
    // First four bytes are the u32 entry count.
    if snap_len < 4 {
        d.count = 0;
        d.pos = 4;
    } else {
        d.count = u32::from_le_bytes(d.snap[0..4].try_into().unwrap()) as usize;
        d.pos = 4;
    }
    d as *mut DIR
}

/// POSIX `readdir(dir)` — next entry or NULL at the end.
#[unsafe(no_mangle)]
pub extern "C" fn readdir(dir: *mut DIR) -> *mut Dirent {
    if dir.is_null() || !is_open(unsafe { &*dir }) {
        errno::set(errno::EBADF);
        return core::ptr::null_mut();
    }
    let d = unsafe { &mut *dir };
    if d.seen >= d.count {
        return core::ptr::null_mut();
    }
    let b = &d.snap[..d.snap_len];
    let mut p = d.pos;
    if p + 4 > d.snap_len {
        d.seen = d.count; // malformed trailing bytes: treat as end
        return core::ptr::null_mut();
    }
    let nlen = u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as usize;
    p += 4;
    if p + nlen + 4 > d.snap_len {
        d.seen = d.count;
        return core::ptr::null_mut();
    }
    let name = &b[p..p + nlen];
    p += nlen;
    let kind = u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
    d.pos = p + 4;

    d.cur.d_ino = 0;
    d.cur.d_type = if kind == 1 { DT_DIR } else { DT_REG };
    let n = core::cmp::min(name.len(), 255);
    d.cur.d_name = [0; 256];
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), d.cur.d_name.as_mut_ptr() as *mut u8, n);
    }
    d.seen += 1;
    &d.cur as *const Dirent as *mut Dirent
}

/// POSIX `closedir(dir)` — release the snapshot.
#[unsafe(no_mangle)]
pub extern "C" fn closedir(dir: *mut DIR) -> c_int {
    if dir.is_null() || !is_open(unsafe { &*dir }) {
        errno::set(errno::EBADF);
        return -1;
    }
    unsafe {
        (&mut *dir).snap_len = 0;
    }
    0
}

/// POSIX `rewinddir(dir)` — reset to the first entry.
#[unsafe(no_mangle)]
pub extern "C" fn rewinddir(dir: *mut DIR) {
    if dir.is_null() || !is_open(unsafe { &*dir }) {
        return;
    }
    let d = unsafe { &mut *dir };
    d.pos = 4;
    d.seen = 0;
}
