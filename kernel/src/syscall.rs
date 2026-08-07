//! Syscall dispatch and handlers.
//!
//! Versioned syscall table: user code passes a table version in R10.
//! Each version is a fixed array of handlers; adding new syscalls in
//! a new version does not break programs compiled against older tables.

use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::serial::SerialPort;
use crate::mm::vmm::Vmm;

/// Syscall function type: (num, arg0, arg1, arg2) -> return value.
pub type SyscallFn = fn(u64, u64, u64, u64) -> u64;

/// Version 1 syscall table: write + exit.
const TABLE_V1: [SyscallFn; 2] = [
    sys_write,  // 0
    sys_exit,   // 1
];

/// Dispatch a syscall by (version, number, args).
pub fn dispatch(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    match 1u64 {
        // Version 1: write + exit
        1 => {
            let idx = num as usize;
            if idx >= TABLE_V1.len() {
                return u64::MAX; // -1: unknown syscall
            }
            TABLE_V1[idx](num, arg0, arg1, arg2)
        }
        _ => u64::MAX, // -1: unknown version
    }
}

/// Top of the x86_64 low (user) canonical half — user pointers must stay below.
const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Copy `len` bytes from a user-space pointer, validating that the whole range
/// lies in the user half and is mapped in the current (user) page tables.
///
/// The syscall handler runs with the user domain's CR3 active, so a raw copy
/// would fault the kernel on a bad pointer; this checks the range up front and
/// returns `None` instead. Returns `None` if the current domain has no address
/// space (not running in a user domain).
fn copy_from_user(ptr: u64, len: usize) -> Option<Vec<u8>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if ptr >= USER_LIMIT {
        return None;
    }
    let end = ptr.checked_add(len as u64)?;
    if end > USER_LIMIT {
        return None;
    }

    // Every page the range touches must be mapped in the current address space.
    let root = crate::obj::domain::current_domain()?.page_root()?;
    let vmm = Vmm::from_root(root);
    let mut page = ptr & !0xFFF;
    while page < end {
        if vmm.translate(page).is_none() {
            return None;
        }
        page += 4096;
    }

    let mut out = vec![0u8; len];
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), len);
    }
    Some(out)
}

/// Syscall 0: write(fd, buf_ptr, len) -> bytes_written or -1.
fn sys_write(_num: u64, fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let len = len as usize;
    if len == 0 {
        return 0;
    }
    if len > 4096 {
        return u64::MAX; // -1: too large
    }

    let buf = match copy_from_user(buf_ptr, len) {
        Some(b) => b,
        None => return u64::MAX, // -1: invalid user buffer
    };

    // Output to serial (COM1) — fd is ignored for now, all writes go to serial.
    let _ = fd;
    for &byte in &buf {
        SerialPort::putc(byte);
    }

    len as u64
}

/// Syscall 1: exit(code) → never returns.
fn sys_exit(_num: u64, code: u64, _arg1: u64, _arg2: u64) -> u64 {
    crate::proc::exit_process(code as i64)
}
