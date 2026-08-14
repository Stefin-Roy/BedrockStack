static mut ERRNO: core::ffi::c_int = 0;

#[unsafe(no_mangle)]
pub extern "C" fn __errno_location() -> *mut core::ffi::c_int {
    unsafe { core::ptr::addr_of_mut!(ERRNO) }
}

/// Convert a raw syscall return: if negative, store -ret as errno and return
/// -1, else return the positive value unchanged.
pub fn ret(ret: isize) -> isize {
    if ret < 0 {
        unsafe {
            ERRNO = (-ret) as core::ffi::c_int;
        }
        -1
    } else {
        ret
    }
}

/// Set errno directly.
pub fn set(err: core::ffi::c_int) {
    unsafe {
        ERRNO = err;
    }
}
