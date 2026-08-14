use crate::syscall::{read_path, write_path};

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

pub fn getpid() -> isize {
    let mut buf = [0u8; 20];
    let r = unsafe { read_path(b"/proc/self/status\0", &mut buf, 0) };
    if r < 0 {
        return r;
    }
    if r < 8 {
        return -1;
    }
    let pid = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    crate::errno::ret(pid as isize)
}

pub fn sched_yield() -> isize {
    let mut buf = [0u8; 0];
    let r = unsafe { write_path(b"/proc/self:yield\0", &mut buf, 0, 0) };
    crate::errno::ret(r)
}

pub fn spawn(path: &str, args: &str) -> isize {
    let mut buf = [0u8; 512];
    let plen = path.len();
    let alen = args.len();
    let total = 8 + plen + alen;
    if total > buf.len() {
        return -1;
    }
    buf[0..4].copy_from_slice(&(plen as u32).to_le_bytes());
    buf[4..4 + plen].copy_from_slice(path.as_bytes());
    buf[4 + plen..8 + plen].copy_from_slice(&(alen as u32).to_le_bytes());
    buf[8 + plen..total].copy_from_slice(args.as_bytes());
    let r = unsafe { write_path(b"/proc/self:spawn\0", &mut buf, total, 0) };
    if r < 0 {
        return crate::errno::ret(r);
    }
    if r < 8 {
        return -1;
    }
    let pid = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    crate::errno::ret(pid as isize)
}

pub fn wait(pid: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&pid.to_le_bytes());
    let r = unsafe { write_path(b"/proc/self:wait\0", &mut buf, 8, 0) };
    if r < 0 {
        return crate::errno::ret(r);
    }
    if r < 8 {
        return -1;
    }
    let code = u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    crate::errno::ret(code as isize)
}

pub fn sleep_ns(ns: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&ns.to_le_bytes());
    let r = unsafe { write_path(b"/kernel/timer:sleep\0", &mut buf, 8, 0) };
    crate::errno::ret(r)
}

pub fn sleep_ms(ms: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&ms.to_le_bytes());
    let r = unsafe { write_path(b"/kernel/timer:sleep_ms\0", &mut buf, 8, 0) };
    crate::errno::ret(r)
}

pub fn sleep(secs: u64) -> isize {
    sleep_ms(secs.saturating_mul(1000))
}

pub fn usleep(usecs: u64) -> isize {
    sleep_ns(usecs.saturating_mul(1000))
}

pub fn kill(pid: u64) -> isize {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&pid.to_le_bytes());
    let r = unsafe { write_path(b"/proc/self:kill\0", &mut buf, 8, 0) };
    crate::errno::ret(r)
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
