use crate::syscall::write_path;

const HEADER: usize = 16;
const MIN_ALIGN: usize = 16;

#[repr(C)]
struct Header {
    size: usize,
    next: *mut Header,
}

static mut FREE: *mut Header = core::ptr::null_mut();
static mut BRK_CUR: usize = 0;

/// `write(/proc/self:brk, {new_break})`; returns the resulting break as a
/// positive isize, or the negative errno.
fn sys_brk(new_break: usize) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&(new_break as u64).to_le_bytes());
    let r = unsafe { write_path(b"/proc/self:brk\0", &mut buf, 8, 0) };
    if r < 0 {
        return r;
    }
    if r < 8 {
        return -1;
    }
    let brk = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    brk as isize
}

/// First-fit allocation of `size` usable bytes (16-aligned payload), growing
/// the heap through `:brk` when no free block fits. Returns a pointer to the
/// usable payload or null.
unsafe fn alloc_bytes(size: usize) -> *mut u8 {
    let raw = size.max(16);
    let need = (raw + MIN_ALIGN - 1) & !(MIN_ALIGN - 1);

    let mut prev: *mut Header = core::ptr::null_mut();
    let mut h = FREE;
    while !h.is_null() {
        let hsize = (*h).size;
        if hsize >= need {
            if hsize - need >= HEADER + 16 {
                let rest = (h as usize + HEADER + need) as *mut Header;
                (*rest).size = hsize - need - HEADER;
                (*rest).next = (*h).next;
                (*h).size = need;
                if prev.is_null() {
                    FREE = rest;
                } else {
                    (*prev).next = rest;
                }
            } else {
                if prev.is_null() {
                    FREE = (*h).next;
                } else {
                    (*prev).next = (*h).next;
                }
            }
            return (h as usize + HEADER) as *mut u8;
        }
        prev = h;
        h = (*h).next;
    }

    if BRK_CUR == 0 {
        let q = sys_brk(0);
        if q < 0 {
            return core::ptr::null_mut();
        }
        BRK_CUR = q as usize;
    }
    let cur = BRK_CUR;
    let grown = sys_brk(cur + HEADER + need);
    if grown < 0 || grown as usize <= cur {
        return core::ptr::null_mut();
    }
    let grown = grown as usize;
    let h = cur as *mut Header;
    (*h).size = need;
    (*h).next = core::ptr::null_mut();
    let alloc = (cur + HEADER) as *mut u8;
    if grown - (cur + HEADER + need) >= HEADER + 16 {
        let rest = (cur + HEADER + need) as *mut Header;
        (*rest).size = grown - (cur + HEADER + need) - HEADER;
        (*rest).next = FREE;
        FREE = rest;
    }
    BRK_CUR = grown;
    alloc
}

#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: usize) -> *mut core::ffi::c_void {
    unsafe { alloc_bytes(size) as *mut core::ffi::c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn calloc(nmemb: usize, size: usize) -> *mut core::ffi::c_void {
    let total = nmemb.saturating_mul(size);
    let p = unsafe { alloc_bytes(total) };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        core::ptr::write_bytes(p, 0, total);
    }
    p as *mut core::ffi::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn realloc(ptr: *mut core::ffi::c_void, new_size: usize) -> *mut core::ffi::c_void {
    if ptr.is_null() {
        return malloc(new_size);
    }
    if new_size == 0 {
        free(ptr);
        return core::ptr::null_mut();
    }
    let hdr = (ptr as usize - HEADER) as *const Header;
    let old = unsafe { (*hdr).size };
    let np = unsafe { alloc_bytes(new_size) };
    if np.is_null() {
        return core::ptr::null_mut();
    }
    let n = core::cmp::min(old, new_size);
    if n > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(ptr as *const u8, np, n);
        }
    }
    free(ptr);
    np as *mut core::ffi::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn free(ptr: *mut core::ffi::c_void) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let h = (ptr as usize - HEADER) as *mut Header;
        (*h).next = FREE;
        FREE = h;
    }
}
