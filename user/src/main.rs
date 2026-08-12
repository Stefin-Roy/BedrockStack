//! BedrockOS first userspace program (`INIT`).
//!
//! Phase 8 demo: write `hello from user space` to `/A/init/test` (tmpfs),
//! read it back and verify, poke `/driver/debugserial` so the bytes land on
//! COM1 directly, then exit. The kernel pre-creates `/A/init/` and
//! `/A/init/test` before launching us, so the first syscall must succeed.
//!
//! Syscall ABI (see kernel/src/arch/x86_64/syscall.rs): number in `rax`,
//! args in `rdi`/`rsi`/`rdx`/`r10`. The return value comes back in `rax`:
//! non-negative = success, negative = -errno. Read it as `isize` — a bare
//! `usize` would turn an error like -ENOENT into a huge garbage number.

#![no_std]
#![no_main]

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
_start:
    and  rsp, -16
    call entry_main
    mov  rdi, rax           # entry_main's status -> exit(code)
    mov  rax, 2
    syscall
    hlt
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── syscall glue ─────────────────────────────────────────────────────

/// Invoke a kernel syscall. `syscall` clobbers both `rcx` and `r11`, so they
/// are declared as dummy outputs; omitting them lets the compiler keep live
/// values in those registers and return garbage. The result is signed: errors
/// come back as negative errnos, which is the whole point of `isize` here.
#[inline]
unsafe fn syscall(n: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
    let ret: isize;
    let _rcx: u64;
    let _r11: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            out("rcx") _rcx,
            out("r11") _r11,
            options(nostack),
        );
    }
    ret
}

const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_EXIT: usize = 2;

// ── demo ─────────────────────────────────────────────────────────────

const PATH: &[u8] = b"/A/init/test";
const MSG: &[u8] = b"hello from user space";
const DEV: &[u8] = b"/driver/debugserial";

/// Pump `msg` out to COM1 via the debugserial device. Returns the -errno on
/// failure so the caller can surface it on the wire too.
fn say(dev: &[u8], msg: &[u8]) -> Result<(), isize> {
    let r = unsafe { syscall(SYS_WRITE, dev.as_ptr() as usize, dev.len(), msg.as_ptr() as usize, msg.len()) };
    if r < 0 {
        Err(r)
    } else {
        Ok(())
    }
}

/// Serialize a syscall failure as `FAIL n` + the complaint, then bail.
fn fail(dev: &[u8], what: &[u8], err: isize, code: usize) -> ! {
    let _ = say(dev, what);
    let mut line = [0u8; 18];
    line[..13].copy_from_slice(b"[user] FAIL: ");
    let n = (err.unsigned_abs() as u64).min(999) as usize;
    line[13] = b'0' + (n / 100) as u8;
    line[14] = b'0' + ((n / 10) % 10) as u8;
    line[15] = b'0' + (n % 10) as u8;
    line[16] = b'\r';
    line[17] = b'\n';
    let _ = say(dev, &line);
    unsafe { syscall(SYS_EXIT, code, 0, 0, 0); }
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn entry_main() -> usize {
    // 1. Write a blob into the tmpfs file the kernel pre-created.
    let wr = unsafe { syscall(SYS_WRITE, PATH.as_ptr() as usize, PATH.len(), MSG.as_ptr() as usize, MSG.len()) };
    if wr < 0 {
        fail(DEV, b"[user] write /A/init/test ", wr, 1);
    }

    // 2. Read it back into a stack buffer and compare.
    let mut buf = [0u8; 64];
    let rd = unsafe { syscall(SYS_READ, PATH.as_ptr() as usize, PATH.len(), buf.as_mut_ptr() as usize, buf.len()) };
    if rd < 0 {
        fail(DEV, b"[user] read /A/init/test ", rd, 2);
    }
    let wr = wr as usize;
    let rd = rd as usize;

    let ok = wr == MSG.len() && rd == MSG.len() && buf[..MSG.len()] == *MSG;
    if !ok {
        fail(DEV, b"[user] write/read MISMATCH", -(core::cmp::max(wr, rd) as isize).max(-99), 3);
    }

    // 3. Prove a plain device path: these bytes hit COM1 right now.
    let _ = say(DEV, b"[user] hello from ring 3\r\n");
    let _ = say(DEV, b"[user] write/read ok\r\n");

    0
}