//! Syscall dispatch and handlers.
//!
//! The ABI is strictly two syscalls over path-based namespaces:
//! - `sys_read` (0): read from a resolved path at an offset into a user buffer.
//! - `sys_write` (1): write a user buffer to a resolved path at an offset.
//!
//! There is no exit syscall: a task terminates by writing to the
//! `/proc/self:exit` op file, which dispatches through the namespace to the
//! synthetic proc tree. Every user-supplied pointer, size, and path is
//! validated up front and never `.unwrap()`/`.expect()`ed — a malformed call
//! returns `u64::MAX` (-1).

use alloc::vec;
use alloc::vec::Vec;

use crate::filesystems::vfs::types::RightsMask;
use crate::mm::vmm::Vmm;

/// Syscall function type: (num, arg0, arg1, arg2, arg3) -> return value.
pub type SyscallFn = fn(u64, u64, u64, u64, u64) -> u64;

/// Version 1 syscall table: read (0), write (1).
const TABLE_V1: [SyscallFn; 2] = [
    sys_read,   // 0
    sys_write,  // 1
];

/// Dispatch a syscall by (version, number, args).
pub fn dispatch(ver: u64, num: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let table: &[SyscallFn] = match ver {
        1 => &TABLE_V1,
        _ => return u64::MAX, // -1: unknown version
    };
    if (num as usize) >= table.len() {
        return u64::MAX; // -1: unknown syscall
    }
    table[num as usize](num, arg0, arg1, arg2, arg3)
}

/// Continuation for an async syscall park: re-dispatches the syscall the task
/// was parked inside using the args still in its parked `UserFrame`. Defined
/// as a plain fn so it can be stored in `Task::parked.continuation`. Async
/// parking is not yet wired for the two-syscall ABI; the machinery is kept so
/// a future phase can park a blocked `read`/`write` and resume it on I/O
/// completion.
fn retry_current_syscall(frame: &mut crate::arch::x86_64::syscall::UserFrame) -> u64 {
    crate::syscall::dispatch(frame.r10, frame.rax, frame.rdi, frame.rsi, frame.rdx, frame.r8)
}

/// The continuation handed to `park_async_retry` by the block layer's async
/// submit path: on resume, re-runs the parked syscall (see
/// [`retry_current_syscall`]), which now collects the completed I/O result.
pub fn syscall_retry_continuation() -> crate::proc::Continuation {
    retry_current_syscall
}

/// Validate that `[ptr, ptr+len)` lies in the user half and is fully mapped in
/// the current (user) page tables. The syscall handler runs with the user
/// domain's CR3 active, so a raw copy would fault the kernel on a bad pointer;
/// this checks the range up front. Returns `false` if the range crosses the
/// user limit, if there is no current task (not running in a user domain), or
/// if any page the range touches is unmapped.
fn user_range_is_mapped(ptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if ptr >= crate::ns::USER_LIMIT {
        return false;
    }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    if end > crate::ns::USER_LIMIT {
        return false;
    }

    // Every page the range touches must be mapped in the current address space.
    let root = match current_domain_page_root() {
        Some(r) => r,
        None => return false,
    };
    let vmm = Vmm::from_root(root);
    let mut page = ptr & !0xFFF;
    while page < end {
        if vmm.translate(page).is_none() {
            return false;
        }
        page += 4096;
    }
    true
}

/// The current task's page-table root, if the scheduler is live.
fn current_domain_page_root() -> Option<u64> {
    let task = crate::proc::current_task()?;
    task.domain.page_root()
}

/// Copy `len` bytes from a user-space pointer, validating that the whole range
/// lies in the user half and is mapped in the current (user) page tables.
///
/// The syscall handler runs with the user domain's CR3 active, so a raw copy
/// would fault the kernel on a bad pointer; this checks the range up front and
/// returns `None` instead. Returns `None` if the current domain has no address
/// space (not running in a user domain).
fn copy_from_user(ptr: u64, len: usize) -> Option<Vec<u8>> {
    if !user_range_is_mapped(ptr, len) {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }

    let mut out = vec![0u8; len];
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), len);
    }
    Some(out)
}

/// Copy `bytes` to a user-space pointer, validating the range exactly as
/// `copy_from_user` does. Returns `false` if the range is not a valid mapped
/// user range, or if the current domain has no address space.
fn copy_to_user(dst: u64, bytes: &[u8]) -> bool {
    if !user_range_is_mapped(dst, bytes.len()) {
        return false;
    }
    if bytes.is_empty() {
        return true;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
    }
    true
}

/// Read a NUL-terminated string from user memory, bounded by `max` bytes,
/// returning the bytes WITHOUT the terminating NUL.
///
/// Walks page by page, validating each page before reading bytes within it, so
/// a short string sitting just before an unmapped page is read successfully.
/// Returns `None` if `ptr` is outside the user half, if no NUL is found within
/// `max` bytes, or if any page is unmapped.
fn read_user_str(ptr: u64, max: usize) -> Option<Vec<u8>> {
    if ptr >= crate::ns::USER_LIMIT {
        return None;
    }
    let mut out = Vec::new();
    let mut page = ptr & !0xFFF;
    if !user_range_is_mapped(page, 1) {
        return None;
    }
    let mut off = 0usize;
    while off < max {
        let addr = ptr.checked_add(off as u64)?;
        if addr >= crate::ns::USER_LIMIT {
            return None;
        }
        // Entering a new page: validate it before reading from it.
        if addr & !0xFFF != page {
            page = addr & !0xFFF;
            if !user_range_is_mapped(page, 1) {
                return None;
            }
        }
        let b = unsafe { core::ptr::read(addr as *const u8) };
        if b == 0 {
            return Some(out);
        }
        out.push(b);
        off += 1;
    }
    None
}

/// Syscall 0: read(path_ptr, offset, buf_ptr, len) -> bytes_read or -1.
///
/// `path_ptr` names a NUL-terminated path resolved through the current task's
/// namespace; the leaf must be readable (binding rights `R`). Synchronous
/// only — the buffer is copied in full before `read` is called.
fn sys_read(_num: u64, path_ptr: u64, offset: u64, buf_ptr: u64, len: u64) -> u64 {
    let Some(path) = read_user_str(path_ptr, crate::ns::MAX_PATH_LEN + 1) else {
        return u64::MAX;
    };
    if len == 0 {
        return 0;
    }
    let len = len as usize;
    if !user_range_is_mapped(buf_ptr, len) {
        return u64::MAX;
    }
    let resolved = match crate::ns::resolve_current(&path) {
        Ok(r) => r,
        Err(_) => return u64::MAX,
    };
    if !resolved.rights.contains(RightsMask::R) {
        return u64::MAX;
    }
    let mut kbuf = vec![0u8; len];
    let n = match resolved.ops.read(offset, &mut kbuf) {
        Ok(n) => n,
        Err(_) => return u64::MAX,
    };
    if !copy_to_user(buf_ptr, &kbuf[..n]) {
        return u64::MAX;
    }
    n as u64
}

/// Syscall 1: write(path_ptr, buf_ptr, len, offset) -> bytes_written or -1.
///
/// `path_ptr` names a NUL-terminated path resolved through the current task's
/// namespace; the leaf must be writable (binding rights `W`).
fn sys_write(_num: u64, path_ptr: u64, buf_ptr: u64, len: u64, offset: u64) -> u64 {
    let Some(path) = read_user_str(path_ptr, crate::ns::MAX_PATH_LEN + 1) else {
        return u64::MAX;
    };
    if len == 0 {
        return 0;
    }
    let len = len as usize;
    if !user_range_is_mapped(buf_ptr, len) {
        return u64::MAX;
    }
    let resolved = match crate::ns::resolve_current(&path) {
        Ok(r) => r,
        Err(_) => return u64::MAX,
    };
    if !resolved.rights.contains(RightsMask::W) {
        return u64::MAX;
    }
    let data = match copy_from_user(buf_ptr, len) {
        Some(d) => d,
        None => return u64::MAX,
    };
    match resolved.ops.write(offset, &data) {
        Ok(n) => n as u64,
        Err(_) => u64::MAX,
    }
}
