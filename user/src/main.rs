//! BedrockOS first userspace program (`INIT`).
//!
//! Phase 9 demo: write `hello from user space` to `/A/init/test` (tmpfs),
//! read it back and verify, poke `/driver/debugserial` so the bytes land on
//! COM1 directly, then exit. The kernel pre-creates `/A/init/` and
//! `/A/init/test` before launching us, so the first syscall must succeed.
//!
//! Final syscall ABI (see kernel/src/arch/x86_64/syscall.rs):
//!   0  read(path, buf, buf_len[, arg4])  — `path` is a NUL-terminated C string.
//!   1  write(path, buf, buf_len[, arg4]) — same registers; the buffer is an
//!                                          in-place request/response area. The
//!                                          input is decoded first, then the
//!                                          provider's output (or error detail)
//!                                          is rewritten into it from byte 0,
//!                                          zero-filled past it. rax = number of
//!                                          *output* bytes (0 for a plain value
//!                                          write), `< 0` = -errno.
//! `arg4` (r10) is an optional provider-defined flags word; `0` = plain value
//! read/write. The VFS file object reads it as a read-at offset, and for
//! writes as append (bit 0) / write-at (bits 8..63).
//! There is no exit syscall: exit via `write(/proc/self:exit, {code})`, which
//! diverges (the current task parks and is reaped by the idle loop).

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
    mov  rdi, rax           # entry_main status -> exit code
    call exit_process       # write /proc/self:exit, never returns
    hlt
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── syscall glue ─────────────────────────────────────────────────────

/// Invoke a kernel syscall. rdi/rsi/rdx = primary args, r10 = the optional
/// provider-defined `arg4`/flags word (`0` = plain value read/write; the VFS
/// file object reads it as read-at offset / append / write-at). `syscall`
/// clobbers both `rcx` and `r11`, so they are declared as dummy outputs;
/// omitting them lets the compiler keep live values in those registers and
/// return garbage. The result is signed: errors come back as negative errnos,
/// which is the whole point of `isize` here.
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

// ── demo ─────────────────────────────────────────────────────────────
//
// NUL-terminated paths (no length argument). The write buffer is consumed in
// place, so every write goes through a writable stack copy — never a .rodata
// constant (writes also zero-fill the buffer, and .rodata may be mapped
// read-only via W^X).

const PATH: &[u8] = b"/A/init/test\0";
const MSG: &[u8] = b"hello from user space";
const DEV: &[u8] = b"/driver/debugserial\0";
const EXIT_PATH: &[u8] = b"/proc/self:exit\0";

/// Pump `msg` out to COM1 via the debugserial device. Returns the -errno on
/// failure so the caller can surface it on the wire too.
fn say(dev: &[u8], msg: &[u8]) -> Result<(), isize> {
    let mut wbuf = [0u8; 64];
    let n = core::cmp::min(msg.len(), wbuf.len());
    wbuf[..n].copy_from_slice(&msg[..n]);
    let r = unsafe { syscall(SYS_WRITE, dev.as_ptr() as usize, wbuf.as_mut_ptr() as usize, n, 0) };
    if r >= 0 {
        Ok(())
    } else {
        Err(r)
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
    exit_process(code)
}

/// Terminate the current task via the /proc method (the exit syscall is gone).
#[unsafe(no_mangle)]
extern "C" fn exit_process(code: usize) -> ! {
    let mut path = [0u8; 16];
    let mut payload = [0u8; 8];
    path[..EXIT_PATH.len()].copy_from_slice(EXIT_PATH);
    payload.copy_from_slice(&(code as u64).to_le_bytes());
    unsafe {
        syscall(SYS_WRITE, path.as_ptr() as usize, payload.as_mut_ptr() as usize, payload.len(), 0);
    }
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn entry_main() -> usize {
    // 1. Write a blob into the tmpfs file the kernel pre-created. The write
    //    buffer is consumed in place (output overwrites it, zero-filled), so
    //    the demo MSG copy lives on the stack; MSG itself stays intact for
    //    the read-back comparison. A plain blob value write has no output, so
    //    success is `>= 0`, not `== MSG.len()`.
    let mut wbuf = [0u8; 64];
    let msg_len = MSG.len();
    wbuf[..msg_len].copy_from_slice(MSG);
    let wr = unsafe { syscall(SYS_WRITE, PATH.as_ptr() as usize, wbuf.as_mut_ptr() as usize, msg_len, 0) };
    if wr < 0 {
        fail(DEV, b"[user] write /A/init/test ", wr, 1);
    }

    // 2. Read it back into a stack buffer and compare with the original.
    let mut buf = [0u8; 64];
    let rd = unsafe { syscall(SYS_READ, PATH.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len(), 0) };
    if rd < 0 {
        fail(DEV, b"[user] read /A/init/test ", rd, 2);
    }
    let rd_len = rd as usize;
    let ok = rd_len == MSG.len() && buf[..MSG.len()] == *MSG;
    if !ok {
        fail(DEV, b"[user] write/read MISMATCH", -(rd_len.max(1) as isize), 3);
    }

    // 3. Prove a plain device path: these bytes hit COM1 right now.
    let _ = say(DEV, b"[user] hello from ring 3\r\n");
    let _ = say(DEV, b"[user] write/read ok\r\n");

    0
}