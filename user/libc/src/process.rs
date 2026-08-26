pub use crate::caps::Cap;
use crate::errno;
use crate::syscall::{read_path, write_path};

static mut LAST_SPAWNED: u64 = 0;

pub use crate::caps::{OwnedCap, CapSet, R as CAP_R, RW as CAP_RW, has_cap};

/// Read the full `/proc/self/status` snapshot into `buf` (28 bytes).
fn read_status() -> [u8; 32] {
    let mut buf = [0u8; 32];
    let r = unsafe { read_path(b"/proc/self/status\0", &mut buf, 0) };
    if r < 28 {
        return [0; 32];
    }
    buf
}

pub fn exit(code: usize) -> ! {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&(code as u64).to_le_bytes());
    unsafe {
        write_path(b"/proc/self:exit\0", &mut buf, 8, 0);
    }
    loop {}
}

pub fn abort() -> ! {
    exit(134)
}

/// POSIX `getpid()`.
#[unsafe(no_mangle)]
pub extern "C" fn getpid() -> crate::ffi::c_int {
    let buf = read_status();
    let pid = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    pid as crate::ffi::c_int
}

/// POSIX `getppid()` — ppid sits at offset 20 in the extended status wire.
#[unsafe(no_mangle)]
pub extern "C" fn getppid() -> crate::ffi::c_int {
    let buf = read_status();
    let ppid = u64::from_le_bytes(buf[20..28].try_into().unwrap());
    ppid as crate::ffi::c_int
}

/// Spawn with explicit capability subset — `caps` must be subset of caller's caps.
/// Replaces the legacy `spawn` (which is removed); every spawn now requires an explicit
/// cap list. Encodes `struct{path:str, args:str, caps:list<{path:str,method:str,perm:u32}>}`
/// into a heap buffer and invokes `/proc/self:spawn_caps`. `perm` must be 1 (R) or 3 (RW).
pub fn spawn(path: &str, args: &str, caps: &[Cap]) -> isize {
    // Estimate: 4+plen +4+alen +4 + caps*(4+path +4+method +4)
    let mut cap_bytes = 0usize;
    for c in caps {
        cap_bytes = cap_bytes.saturating_add(12 + c.path.len() + c.method.unwrap_or("").len());
    }
    let total = 8 + path.len() + args.len() + 4 + cap_bytes;
    // Guard against hostile huge lists that would OOM the heap.
    if caps.len() > 8192 || total > 8192 {
        return -1;
    }
    // Use stack buffer to avoid heap OOM abort (Vec allocation would abort on OOM via global oom handler).
    // total is bounded 8192, fits on stack; use static scratch via heap fallback only if needed.
    let mut stack_buf = [0u8; 8192];
    let mut off = 0usize;
    stack_buf[off..off+4].copy_from_slice(&(path.len() as u32).to_le_bytes()); off+=4;
    stack_buf[off..off+path.len()].copy_from_slice(path.as_bytes()); off+=path.len();
    stack_buf[off..off+4].copy_from_slice(&(args.len() as u32).to_le_bytes()); off+=4;
    stack_buf[off..off+args.len()].copy_from_slice(args.as_bytes()); off+=args.len();
    stack_buf[off..off+4].copy_from_slice(&(caps.len() as u32).to_le_bytes()); off+=4;
    for c in caps {
        stack_buf[off..off+4].copy_from_slice(&(c.path.len() as u32).to_le_bytes()); off+=4;
        stack_buf[off..off+c.path.len()].copy_from_slice(c.path.as_bytes()); off+=c.path.len();
        let m = c.method.unwrap_or("");
        stack_buf[off..off+4].copy_from_slice(&(m.len() as u32).to_le_bytes()); off+=4;
        stack_buf[off..off+m.len()].copy_from_slice(m.as_bytes()); off+=m.len();
        stack_buf[off..off+4].copy_from_slice(&(c.perm as u32).to_le_bytes()); off+=4;
    }
    let len = off;
    let r = unsafe { write_path(b"/proc/self:spawn_caps\0", &mut stack_buf[..len], len, 0) };
    if r < 0 {
        return errno::ret(r);
    }
    if r < 8 {
        return -1;
    }
    let pid = u64::from_le_bytes([
        stack_buf[0], stack_buf[1], stack_buf[2], stack_buf[3], stack_buf[4], stack_buf[5], stack_buf[6], stack_buf[7],
    ]);
    unsafe {
        LAST_SPAWNED = pid;
    }
    errno::ret(pid as isize)
}

pub fn wait_rs(pid: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&pid.to_le_bytes());
    let r = unsafe { write_path(b"/proc/self:wait\0", &mut buf, 8, 0) };
    if r < 0 {
        return errno::ret(r);
    }
    if r < 8 {
        return -1;
    }
    let code = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    errno::ret(code as isize)
}

pub fn wait(pid: u64) -> isize {
    wait_rs(pid)
}

/// POSIX `wait(&status)` — wait for the most recently spawned child.
#[unsafe(export_name = "wait")]
pub extern "C" fn c_wait(status: *mut crate::ffi::c_int) -> crate::ffi::c_int {
    waitpid(-1, status, 0)
}

/// POSIX `waitpid(pid, &status, options)`.  `pid` < 0 or 0 selects the last
/// spawned child (approximation of "any child"); options are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn waitpid(
    pid: crate::ffi::c_int,
    status: *mut crate::ffi::c_int,
    _options: crate::ffi::c_int,
) -> crate::ffi::c_int {
    let target: u64 = if pid < 0 || pid == 0 {
        unsafe { LAST_SPAWNED }
    } else {
        pid as u64
    };
    if target == 0 {
        errno::set(errno::ECHILD);
        return -1;
    }
    let r = wait_rs(target);
    if r < 0 {
        return -1;
    }
    // Encode POSIX status: exited normally, low 8 bits = code.
    if !status.is_null() {
        unsafe {
            *status = (r & 0xFF) as crate::ffi::c_int;
        }
    }
    target as crate::ffi::c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn waitid(_idtype: crate::ffi::c_int, id: crate::ffi::c_int, infop: *mut crate::ffi::c_void, options: crate::ffi::c_int) -> crate::ffi::c_int {
    let _ = infop;
    let _ = options;
    let r = waitpid(id, core::ptr::null_mut(), 0);
    if r < 0 { -1 } else { 0 }
}

/// POSIX `kill(pid, sig)` — the kernel parks the target; signal numbers are
/// ignored (the task is simply ended).
#[unsafe(no_mangle)]
pub extern "C" fn kill(pid: crate::ffi::c_int, _sig: crate::ffi::c_int) -> crate::ffi::c_int {
    if pid <= 0 {
        errno::set(errno::EINVAL);
        return -1;
    }
    let r = kill_rs(pid as u64);
    if r < 0 { -1 } else { 0 }
}

fn kill_rs(pid: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&pid.to_le_bytes());
    let r = unsafe { write_path(b"/proc/self:kill\0", &mut buf, 8, 0) };
    errno::ret(r)
}

pub fn sleep_ns(ns: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&ns.to_le_bytes());
    let r = unsafe { write_path(b"/kernel/timer:sleep\0", &mut buf, 8, 0) };
    errno::ret(r)
}

pub fn sleep_ms(ms: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&ms.to_le_bytes());
    let r = unsafe { write_path(b"/kernel/timer:sleep_ms\0", &mut buf, 8, 0) };
    errno::ret(r)
}

pub fn sleep(secs: u64) -> isize {
    sleep_ms(secs.saturating_mul(1000))
}

pub fn usleep(usecs: u64) -> isize {
    sleep_ns(usecs.saturating_mul(1000))
}

/// Read `/proc/self/args` (a `str` wire: u32 LE length + payload), skip the
/// length prefix and copy the payload into `buf`. Returns the payload length,
/// or -1 on error or if it does not fit.
pub fn args(buf: &mut [u8]) -> isize {
    let r = unsafe { read_path(b"/proc/self/args\0", buf, 0) };
    if r < 0 {
        return -1;
    }
    let n = r as usize;
    if n < 4 {
        return -1;
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if 4 + len > n || len > buf.len() {
        return -1;
    }
    buf.copy_within(4..4 + len, 0);
    len as isize
}
