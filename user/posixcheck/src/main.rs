//! `posixcheck` — a conformance harness for the permissive `user/libc`.
//!
//! Runs C-ABI checks (`src/checks.c`, compiled by `build.rs` against
//! `user/libc/include`) plus Rust-API checks here, and prints one PASS/FAIL
//! line per case to stdout (routed to serial via the kernel's std-stream
//! monitor).  Exits 0 when every check passes, else the failure count.

#![no_std]
#![no_main]

use core::ffi::c_int;

#[link(name = "checks", kind = "static", modifiers = "+whole-archive")]
unsafe extern "C" {
    fn run_checks() -> c_int;
}

fn posix_child_caps() -> &'static [libc::process::Cap<'static>] {
    &[
        libc::process::Cap { path: "proc", method: None, perm: 3 },
        libc::process::Cap { path: "proc/self", method: None, perm: 3 },
        libc::process::Cap { path: "proc/self", method: Some("exit"), perm: 3 },
        libc::process::Cap { path: "proc/self", method: Some("brk"), perm: 3 },
        libc::process::Cap { path: "proc/self", method: Some("mmap"), perm: 3 },
        libc::process::Cap { path: "proc/self", method: Some("munmap"), perm: 3 },
        libc::process::Cap { path: "proc/self/args", method: None, perm: 1 },
        libc::process::Cap { path: "proc/self/caps", method: None, perm: 1 },
        libc::process::Cap { path: "proc/self/std", method: None, perm: 3 },
        libc::process::Cap { path: "proc/self/std/out", method: None, perm: 3 },
        libc::process::Cap { path: "proc/self/std/err", method: None, perm: 3 },
    ]
}

/// Rust-API side: spawn a copy of ourselves as a child and verify the
/// parent/child relationship end to end.
fn rust_checks() -> c_int {
    let mut fails = 0;

    let pid = libc::process::getpid();
    let ppid = libc::process::getppid();
    if pid > 0 && ppid > 0 && ppid != pid {
        libc::stdio::puts(c"RUST PASS pid/ppid".as_ptr());
    } else {
        libc::stdio::puts(c"RUST FAIL pid/ppid".as_ptr());
        fails += 1;
    }

    // caps introspection via /proc/self/caps
    if libc::caps::has_cap("proc/self/caps", None, libc::caps::R) {
        libc::stdio::puts(c"RUST PASS caps has_cap".as_ptr());
    } else {
        libc::stdio::puts(c"RUST FAIL caps has_cap".as_ptr());
        fails += 1;
    }
    match libc::caps::current_caps() {
        Ok(v) if !v.is_empty() => {
            libc::stdio::puts(c"RUST PASS caps list".as_ptr());
        }
        _ => {
            libc::stdio::puts(c"RUST FAIL caps list".as_ptr());
            fails += 1;
        }
    }

    let child = libc::process::spawn("/B/EFI/BEDROCK/POSIXCHECK", "child", posix_child_caps());
    if child > 0 {
        let code = libc::process::wait(child as u64);
        if code == 7 {
            libc::stdio::puts(c"RUST PASS spawn/wait".as_ptr());
        } else {
            libc::stdio::puts(c"RUST FAIL spawn/wait".as_ptr());
            fails += 1;
        }
    } else {
        libc::stdio::puts(c"RUST FAIL spawn".as_ptr());
        fails += 1;
    }

    fails
}

#[unsafe(no_mangle)]
pub extern "C" fn entry_main() -> usize {
    // DIAG: does the FILE-based printf path write to the captured stdout at all?
    let pr = unsafe { libc::stdio::printf(c"POSIXCHECK printf-ping rc=%d\n".as_ptr(), 1234) };
    let mut ping = [0u8; 64];
    let msg = b"POSIXCHECK write-ping\n";
    ping[..msg.len()].copy_from_slice(msg);
    let _ = libc::stdio::write(1, ping.as_ptr() as *const core::ffi::c_void, msg.len());
    libc::stdio::puts(c"POSIXCHECK puts-ping".as_ptr());
    let _ = pr;

    // Child role: verify our args then exit with a distinctive code.
    let mut abuf = [0u8; 64];
    let nargs = libc::process::args(&mut abuf);
    if nargs >= 0 && &abuf[..nargs as usize] == b"child" {
        libc::process::exit(7);
    }

    let cfails = unsafe { run_checks() };
    let rfails = rust_checks();
    let total = cfails + rfails;
    if total == 0 {
        libc::stdio::puts(c"POSIXCHECK: ALL PASS".as_ptr());
    } else {
        unsafe {
            libc::stdio::printf(c"POSIXCHECK: %d FAILURE(S)\n".as_ptr(), total);
        }
    }
    total as usize
}
