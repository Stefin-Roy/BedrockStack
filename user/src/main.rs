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
const SPAWN_PATH: &[u8] = b"/proc/self:spawn\0";
const WAIT_PATH: &[u8] = b"/proc/self:wait\0";
const ARGS_PATH: &[u8] = b"/proc/self/args\0";
/// The INIT binary's ESP path as a `:spawn` payload string (no NUL — the
/// schema payload is a length-prefixed string, not a C string).
const SELF_ELF: &[u8] = b"/B/EFI/BEDROCK/INIT";
/// The exit code the child uses to prove `:wait` retains and returns it.
const CHILD_EXIT: u64 = 42;

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

/// Read `/proc/self/args` and decode its `str` value into `buf`.  Returns the
/// argument bytes on success, or the negative errno.
fn read_self_args(buf: &mut [u8]) -> Result<&[u8], isize> {
    let r = unsafe {
        syscall(SYS_READ, ARGS_PATH.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len(), 0)
    };
    if r < 0 {
        return Err(r);
    }
    let n = r as usize;
    if n < 4 {
        return Err(-1);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if 4 + len > n {
        return Err(-1);
    }
    Ok(&buf[4..4 + len])
}

/// `write(/proc/self:spawn, {path: "/B/EFI/BEDROCK/INIT", args: "child"})`.
/// On success writes the new pid to `out` and returns 0, else the -errno.
fn spawn_child(out: &mut [u8; 8]) -> isize {
    let args: &[u8] = b"child";
    let mut payload = [0u8; 128];
    let plen = SELF_ELF.len();
    let alen = args.len();
    let total = 8 + plen + alen;
    if total > payload.len() {
        return -(1);
    }
    payload[0..4].copy_from_slice(&(plen as u32).to_le_bytes());
    payload[4..4 + plen].copy_from_slice(SELF_ELF);
    payload[4 + plen..8 + plen].copy_from_slice(&(alen as u32).to_le_bytes());
    payload[8 + plen..total].copy_from_slice(args);
    let r = unsafe {
        syscall(SYS_WRITE, SPAWN_PATH.as_ptr() as usize, payload.as_mut_ptr() as usize, total, 0)
    };
    if r < 0 {
        return r;
    }
    if r < 8 {
        return -(1);
    }
    out.copy_from_slice(&payload[..8]);
    0
}

/// `write(/proc/self:wait, {pid})`; blocks until the child exits and returns
/// its exit code on success (>= 0), else the -errno.
fn wait_child(pid: u64) -> isize {
    let mut payload = [0u8; 8];
    payload.copy_from_slice(&pid.to_le_bytes());
    let r = unsafe {
        syscall(SYS_WRITE, WAIT_PATH.as_ptr() as usize, payload.as_mut_ptr() as usize, payload.len(), 0)
    };
    if r < 0 {
        return r;
    }
    if r < 8 {
        return -(1);
    }
    u64::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3],
        payload[4], payload[5], payload[6], payload[7],
    ]) as isize
}

/// Print `[user] <label> <decimal>\r\n` to COM1 via the debugserial device.
fn say_num(dev: &[u8], label: &[u8], n: u64) {
    let mut line = [0u8; 48];
    let mut i = 0;
    line[i..i + label.len()].copy_from_slice(label);
    i += label.len();
    let mut digits = [0u8; 20];
    let mut d = 20;
    let mut v = n;
    if v == 0 {
        digits[19] = b'0';
        d = 19;
    }
    while v > 0 {
        d -= 1;
        digits[d] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    line[i..i + (20 - d)].copy_from_slice(&digits[d..]);
    i += 20 - d;
    line[i] = b'\r';
    line[i + 1] = b'\n';
    let _ = say(dev, &line[..i + 2]);
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
    // Role switch: the supervisor (no args) runs the demo and spawns a child
    // to prove :spawn / :wait / exit-code retention end-to-end; the child
    // (args == "child") verifies its arguments via /proc/self/args and exits
    // with code CHILD_EXIT.
    let mut abuf = [0u8; 64];
    let args = match read_self_args(&mut abuf) {
        Ok(a) => a,
        Err(e) => fail(DEV, b"[user] read args ", e, 4),
    };
    if args == b"child".as_slice() {
        let _ = say(DEV, b"[user] child: args verified\r\n");
        exit_process(CHILD_EXIT as usize);
    }

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

    // 4. Supervisor demo: spawn a copy of ourselves as a child (args="child"),
    //    wait for it, and print its retained exit code.
    let mut pidb = [0u8; 8];
    let sres = spawn_child(&mut pidb);
    if sres < 0 {
        fail(DEV, b"[user] spawn ", sres, 5);
    }
    let pid = u64::from_le_bytes(pidb);
    say_num(DEV, b"[user] spawned child pid=", pid);
    let code = wait_child(pid);
    if code < 0 {
        fail(DEV, b"[user] wait ", code, 6);
    }
    say_num(DEV, b"[user] child exit code=", code as u64);

    0
}