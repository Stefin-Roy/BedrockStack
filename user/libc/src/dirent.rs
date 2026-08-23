//! `<dirent.h>` — directory streams over the kernel's VFS listing wire.
//!
//! `read(dir_path)` returns the encoded `DIR_SCHEMA` listing:
//! `Struct[ List[ Struct[ Str name, Enum kind ] ] ]`, i.e. `u32` count
//! followed by `(u32 namelen, name, u32 kind)` per entry.  We snapshot the
//! listing once in `opendir` and walk it from a `.bss` pool (single-threaded
//! tasks).

use core::ffi::{c_char, c_int, c_long};

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
        let p = core::ptr::addr_of_mut!(DIRS) as *mut DIR;
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
    let plen = unsafe { crate::string::strlen(path) };
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
    let r = unsafe { read_path(&d.path[..plen + 1], &mut d.snap, 0) };
    if r < 0 {
        errno::set((-r) as c_int);
        d.snap_len = 0;
        return core::ptr::null_mut();
    }
    let snap_len = r as usize;
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

#[unsafe(no_mangle)]
pub extern "C" fn seekdir(dir: *mut DIR, loc: c_long) {
    if dir.is_null() || !is_open(unsafe { &*dir }) {
        return;
    }
    let d = unsafe { &mut *dir };
    let target = loc.max(0) as usize;
    if target <= d.count {
        // Re-parse from start to target to reconstruct `pos`.
        d.pos = 4;
        d.seen = 0;
        for _ in 0..target {
            if readdir(dir).is_null() {
                break;
            }
        }
        // Rewind effect already set seen correctly; we want seen == target.
        // readdir advanced seen; we need to ensure pos points to next entry.
        // Our loop above left seen == target, so we revert one extra? Already correct because readdir increments.
        // To implement seek simply set seen=target and re-scan was done.
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn telldir(dir: *mut DIR) -> c_long {
    if dir.is_null() || !is_open(unsafe { &*dir }) {
        crate::errno::set(crate::errno::EBADF);
        return -1;
    }
    unsafe { (*dir).seen as c_long }
}

#[unsafe(no_mangle)]
pub extern "C" fn dirfd(_dir: *mut DIR) -> c_int {
    crate::errno::set(crate::errno::ENOSYS);
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn fdopendir(_fd: c_int) -> *mut DIR {
    crate::errno::set(crate::errno::ENOSYS);
    core::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn alphasort(a: *const *const Dirent, b: *const *const Dirent) -> c_int {
    if a.is_null() || b.is_null() {
        return 0;
    }
    unsafe {
        let da = &**a;
        let db = &**b;
        crate::string::strcmp(da.d_name.as_ptr(), db.d_name.as_ptr())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn scandir(
    dirp: *const c_char,
    namelist: *mut *mut *mut Dirent,
    filter: Option<unsafe extern "C" fn(*const Dirent) -> c_int>,
    compar: Option<unsafe extern "C" fn(*const *const Dirent, *const *const Dirent) -> c_int>,
) -> c_int {
    if dirp.is_null() || namelist.is_null() {
        crate::errno::set(crate::errno::EINVAL);
        return -1;
    }
    let dir = opendir(dirp);
    if dir.is_null() {
        return -1;
    }
    // Collect entries into a Vec-like heap array via realloc.
    let mut cap = 16usize;
    let mut len = 0usize;
    let mut arr = crate::mem::malloc(cap * core::mem::size_of::<*mut Dirent>()) as *mut *mut Dirent;
    if arr.is_null() {
        closedir(dir);
        crate::errno::set(crate::errno::ENOMEM);
        return -1;
    }
    loop {
        let de = readdir(dir);
        if de.is_null() {
            break;
        }
        // Apply filter if present.
        if let Some(f) = filter {
            let keep = unsafe { f(de as *const Dirent) };
            if keep == 0 {
                continue;
            }
        }
        if len >= cap {
            let newcap = cap * 2;
            let np = crate::mem::realloc(arr as *mut core::ffi::c_void, newcap * core::mem::size_of::<*mut Dirent>()) as *mut *mut Dirent;
            if np.is_null() {
                // cleanup
                for i in 0..len {
                    crate::mem::free(unsafe { *arr.add(i) } as *mut core::ffi::c_void);
                }
                crate::mem::free(arr as *mut core::ffi::c_void);
                closedir(dir);
                crate::errno::set(crate::errno::ENOMEM);
                return -1;
            }
            arr = np;
            cap = newcap;
        }
        // Duplicate Dirent onto heap.
        let dup = crate::mem::malloc(core::mem::size_of::<Dirent>()) as *mut Dirent;
        if dup.is_null() {
            for i in 0..len {
                crate::mem::free(unsafe { *arr.add(i) } as *mut core::ffi::c_void);
            }
            crate::mem::free(arr as *mut core::ffi::c_void);
            closedir(dir);
            crate::errno::set(crate::errno::ENOMEM);
            return -1;
        }
        unsafe {
            *dup = *de;
            *arr.add(len) = dup;
        }
        len += 1;
    }
    closedir(dir);
    if let Some(cmp) = compar {
        // Simple insertion sort via comparator.
        unsafe {
            for i in 1..len {
                let mut j = i;
                while j > 0 && cmp(arr.add(j) as *const *const Dirent, arr.add(j - 1) as *const *const Dirent) < 0 {
                    let tmp = *arr.add(j);
                    *arr.add(j) = *arr.add(j - 1);
                    *arr.add(j - 1) = tmp;
                    j -= 1;
                }
            }
        }
    }
    unsafe { *namelist = arr };
    len as c_int
}
