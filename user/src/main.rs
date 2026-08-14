//! BedrockOS first userspace program (`INIT`), built on the `libc` crate.
//!
//! Writes everything to `/proc/self/std/out`; the kernel boot path reads it
//! back and prints it to serial after INIT exits (see `task/load.rs`). The
//! supervisor (no args) runs the demo, spawns a copy of itself as a child
//! (args = "child"), waits for it, and echoes the child's captured stdout —
//! proving the per-process std streams end to end.

#![no_std]
#![no_main]

/// Convert a byte array into a `&[u8]` path (already NUL-terminated).
#[unsafe(no_mangle)]
pub extern "C" fn entry_main() -> usize {
    // Role switch: the child (args == "child") verifies its arguments and
    // exits with code 42; the supervisor runs the demo below.
    let mut abuf = [0u8; 64];
    let nargs = libc::process::args(&mut abuf);
    if nargs >= 0 && &abuf[..nargs as usize] == b"child" {
        libc::stdio::puts(c"child: args verified, exiting".as_ptr());
        libc::process::exit(42);
    }

    unsafe {
        libc::stdio::printf(c"hello from user space (pid=%d)\n".as_ptr(), libc::process::getpid());
    }

    // 1. Write a blob into the tmpfs file the kernel pre-created, read it back
    //    and verify. The write buffer is consumed in place (zero-filled), so
    //    the payload lives in a stack copy.
    let msg: &[u8] = b"hello from user space";
    let mut wbuf = [0u8; 64];
    wbuf[..msg.len()].copy_from_slice(msg);
    let wr = unsafe { libc::syscall::write_path(b"/A/init/test\0", &mut wbuf, msg.len(), 0) };
    if wr < 0 {
        libc::stdio::puts(c"write /A/init/test failed".as_ptr());
        return 1;
    }
    let mut rbuf = [0u8; 64];
    let rd = unsafe { libc::syscall::read_path(b"/A/init/test\0", &mut rbuf, 0) };
    if rd < 0 || rd as usize != msg.len() || rbuf[..msg.len()] != *msg {
        libc::stdio::puts(c"write/read MISMATCH".as_ptr());
        return 3;
    }
    libc::stdio::puts(c"write/read ok".as_ptr());

    // 2. Spawn a copy of ourselves as a child (args="child"), wait for it, and
    //    echo its captured stdout. The child's std/out stream survives as long
    //    as its /proc dir, so it is readable after :wait until the idle reaper
    //    runs.
    let pid = libc::process::spawn("/B/EFI/BEDROCK/INIT", "child");
    if pid < 0 {
        libc::stdio::puts(c"spawn failed".as_ptr());
        return 5;
    }
    unsafe {
        libc::stdio::printf(c"spawned child pid=%d\n".as_ptr(), pid);
    }
    let code = libc::process::wait(pid as u64);
    unsafe {
        libc::stdio::printf(c"child exit code=%d\n".as_ptr(), code);
    }

    // 3. Read the child's stdout back and echo it.
    let mut spath = [0u8; 32];
    let mut slen = 0usize;
    for &b in b"/proc/" {
        spath[slen] = b;
        slen += 1;
    }
    let mut digits = [0u8; 20];
    let mut d = 20usize;
    let mut v = pid as u64;
    if v == 0 {
        digits[19] = b'0';
        d = 19;
    }
    while v > 0 {
        d -= 1;
        digits[d] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for i in d..20 {
        spath[slen] = digits[i];
        slen += 1;
    }
    for &b in b"/std/out\0" {
        spath[slen] = b;
        slen += 1;
    }
    let mut sbuf = [0u8; 128];
    let sr = unsafe { libc::syscall::read_path(&spath[..slen], &mut sbuf, 0) };
    if sr >= 0 {
        unsafe {
            libc::stdio::printf(
                c"child stdout: %s\n".as_ptr(),
                sbuf.as_ptr() as *const core::ffi::c_char,
            );
        }
    } else {
        libc::stdio::puts(c"read child stdout failed".as_ptr());
    }

    0
}
