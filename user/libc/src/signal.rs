//! `<signal.h>` — stubs.  The kernel delivers no signals (tasks can only
//! self-terminate), so handlers are accepted but never invoked.  `raise`
//! maps onto `kill`.

use core::ffi::{c_int, c_uint, c_void};

use crate::errno;

/// `SIG_DFL` / `SIG_IGN` constants (value-compatible with glibc's `(void*)0` /
/// `(void*)1`).
pub const SIG_DFL: *mut c_void = core::ptr::null_mut();
pub const SIG_IGN: *mut c_void = 1 as *mut c_void;
pub const SIG_ERR: *mut c_void = usize::MAX as *mut c_void;

pub const SIG_DFL_U: c_uint = 0;
pub const SIG_IGN_U: c_uint = 1;
pub const SIG_ERR_U: c_uint = usize::MAX as c_uint;

/// `signal(sig, handler)` — accept and ignore; never delivered.
#[unsafe(no_mangle)]
pub extern "C" fn signal(_sig: c_int, _handler: *mut c_void) -> *mut c_void {
    SIG_DFL
}

/// `raise(sig)` — the kernel can only kill; treat any signal as `SIGKILL`.
#[unsafe(no_mangle)]
pub extern "C" fn raise(_sig: c_int) -> c_int {
    crate::process::kill(crate::process::getpid(), 9)
}

/// `alarm(seconds)` — no timers; returns 0 (no prior alarm).
#[unsafe(no_mangle)]
pub extern "C" fn alarm(_seconds: c_uint) -> c_uint {
    0
}

// ── sigaction set (opaque, accepted but inert) ────────────────────────

/// Opaque signal set — one `c_ulong` bitmask word.
#[repr(C)]
pub struct SigSet {
    pub bits: [usize; 1],
}

#[unsafe(no_mangle)]
pub extern "C" fn sigemptyset(set: *mut SigSet) -> c_int {
    if set.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        (*set).bits = [0; 1];
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn sigfillset(set: *mut SigSet) -> c_int {
    if set.is_null() {
        errno::set(errno::EINVAL);
        return -1;
    }
    unsafe {
        (*set).bits = [usize::MAX; 1];
    }
    0
}

/// `sigaction()` — record nothing; always succeeds.
#[unsafe(no_mangle)]
pub extern "C" fn sigaction(
    _sig: c_int,
    _act: *const c_void,
    _oldact: *mut c_void,
) -> c_int {
    0
}