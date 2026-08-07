//! BedrockOS userspace init.
//!
//! Runs in ring 3 via `sysretq`. Exercises the syscall ABI (version 1 table:
//! syscall number in RAX, args in RDI/RSI/RDX, table version in R10) and drives
//! the capability model end-to-end from ring 3 — serial, mounts, dirs, physmem
//! surface reads, dup, and registry discovery — then exits. GS is kernel-owned
//! — user code must never touch it.

#![no_std]
#![no_main]

use core::arch::asm;

/// Syscall numbers (version 1 table).
const SYS_WRITE: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_INVOKE: u64 = 2;
const SYS_CONTRACT_ID: u64 = 3;
const SYS_CAP_DUP: u64 = 4;
const SYS_CAP_QUERY: u64 = 5;
const SYS_CAP_DELEGATE: u64 = 6;

/// Table version passed in R10 on every syscall.
const TABLE_VERSION: u64 = 1;

/// Invoke reply status: 0 = Ok.
const REPLY_OK: u64 = 0;

/// Descriptor argument tags.
const TAG_U64: u64 = 0;
const TAG_BUF: u64 = 1;
const TAG_STR: u64 = 2;

/// Endowed capability ids (insertion order — process ABI, not resolved).
const CAP_SERIAL: u64 = 0;
const CAP_MOUNT: u64 = 1;
const CAP_REGISTRY: u64 = 2;
const CAP_PHYSMEM: u64 = 3;

/// FNV-1a constants, mirroring `kernel/src/obj/hook.rs`.
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// FNV-1a over a hook name, matching the kernel's `HookId::of`.
const fn hook_id(name: &'static str) -> u64 {
    let bytes = name.as_bytes();
    let mut h = FNV_OFFSET;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h
}

/// Entry point jumped to by the kernel (sysretq from ring 0).
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Resolve the contracts we are about to invoke through. physmem is only
    // queried by cap id below, so its contract id is deliberately unbound.
    let cid_serial = resolve(b"serial-console\0");
    let cid_mount = resolve(b"fs:mount\0");
    let cid_dir = resolve(b"fs:dir\0");
    let _cid_physmem = resolve(b"physmem:allocation\0");
    let cid_registry = resolve(b"infra:registry\0");

    // 1. Serial via cap (replaces ambient write).
    puts_cap(CAP_SERIAL, cid_serial, b"Hello from ring 3 via serial cap!\n");

    let mut desc = [0u8; 512];
    let mut reply = [0u8; 4096];

    // 2. Mount tmpfs on A> — resolves idempotently to the A> root DirNode.
    let _ = build_desc(
        &mut desc,
        CAP_MOUNT,
        cid_mount,
        hook_id("mount"),
        &[DescArg::Str(b"tmpfs"), DescArg::U64(0)],
    );
    let (_, ncaps, mut cur) = invoke_reply(&mut desc, &mut reply, 4096, CAP_SERIAL, cid_serial);
    if ncaps < 1 {
        die(CAP_SERIAL, cid_serial, b"[init] mount: expected a cap\n");
    }
    let dir_cap = read_u64(&reply, &mut cur);
    puts_cap(CAP_SERIAL, cid_serial, b"[init] mounted A>\n");

    // 3. mkdir userland.
    let _ = build_desc(&mut desc, dir_cap, cid_dir, hook_id("mkdir"), &[DescArg::Str(b"userland")]);
    let _ = invoke_reply(&mut desc, &mut reply, 4096, CAP_SERIAL, cid_serial);
    puts_cap(CAP_SERIAL, cid_serial, b"[init] mkdir userland ok\n");

    // 4. label — the reply carries one Str value.
    let _ = build_desc(&mut desc, dir_cap, cid_dir, hook_id("label"), &[]);
    let (nvalues, _, mut cur) = invoke_reply(&mut desc, &mut reply, 4096, CAP_SERIAL, cid_serial);
    if nvalues < 1 {
        die(CAP_SERIAL, cid_serial, b"[init] label: expected a value\n");
    }
    let tag = read_u64(&reply, &mut cur);
    if tag != TAG_STR {
        die(CAP_SERIAL, cid_serial, b"[init] label: expected a Str value\n");
    }
    let slen = read_u64(&reply, &mut cur) as usize;
    puts_cap(CAP_SERIAL, cid_serial, b"[init] label: ");
    print_str(CAP_SERIAL, cid_serial, &reply[cur..cur + slen]);
    puts_cap(CAP_SERIAL, cid_serial, b"\n");

    // 5. traverse into userland.
    let _ = build_desc(&mut desc, dir_cap, cid_dir, hook_id("traverse"), &[DescArg::Str(b"userland")]);
    let (_, ncaps, mut cur) = invoke_reply(&mut desc, &mut reply, 4096, CAP_SERIAL, cid_serial);
    if ncaps < 1 {
        die(CAP_SERIAL, cid_serial, b"[init] traverse: expected a cap\n");
    }
    let child_cap = read_u64(&reply, &mut cur);
    puts_cap(CAP_SERIAL, cid_serial, b"[init] traversed to userland (cap ");
    print_u64(CAP_SERIAL, cid_serial, child_cap);

    // 6. Query the physmem surface.
    let query = b"total_frames\0";
    let frames = cap_query(CAP_PHYSMEM, query.as_ptr() as u64);
    if frames == u64::MAX {
        die(CAP_SERIAL, cid_serial, b"[init] cap_query(total_frames) failed\n");
    }
    puts_cap(CAP_SERIAL, cid_serial, b"[init] physmem total_frames: ");
    print_u64(CAP_SERIAL, cid_serial, frames);

    // 7. Dup the serial cap and use it.
    let dup_serial = cap_dup(CAP_SERIAL);
    if dup_serial == u64::MAX {
        die(CAP_SERIAL, cid_serial, b"[init] cap_dup failed\n");
    }
    puts_cap(dup_serial, cid_serial, b"[init] via dup'd cap\n");

    // 8. Registry discovery — the reply carries two Str values (name, doc).
    let _ = build_desc(&mut desc, CAP_REGISTRY, cid_registry, hook_id("lookup"), &[DescArg::U64(cid_mount)]);
    let (nvalues, _, mut cur) = invoke_reply(&mut desc, &mut reply, 4096, CAP_SERIAL, cid_serial);
    let mut i = 0;
    while i < nvalues {
        let tag = read_u64(&reply, &mut cur);
        if tag != TAG_STR {
            die(CAP_SERIAL, cid_serial, b"[init] lookup: expected a Str value\n");
        }
        let slen = read_u64(&reply, &mut cur) as usize;
        puts_cap(CAP_SERIAL, cid_serial, &reply[cur..cur + slen]);
        puts_cap(CAP_SERIAL, cid_serial, b"\n");
        cur += slen;
        i += 1;
    }

    // 9. Exit back into the kernel idle loop.
    unsafe {
        syscall(SYS_EXIT, 0, 0, 0);
    }
    unreachable!("sys_exit never returns");
}

/// Resolve a contract name to an id (NUL-terminated), or print + exit(1).
fn resolve(name: &'static [u8]) -> u64 {
    let id = contract_id(name.as_ptr() as u64);
    if id == u64::MAX {
        unsafe {
            syscall(SYS_WRITE, 1, b"[init] unknown contract: ".as_ptr() as u64, 25);
            syscall(SYS_WRITE, 1, name.as_ptr() as u64, (name.len() - 1) as u64);
            syscall(SYS_WRITE, 1, b"\n".as_ptr() as u64, 1);
            syscall(SYS_EXIT, 1, 0, 0);
        }
        unreachable!()
    }
    id
}

/// Raw `syscall` invocation.
///
/// # Safety
/// `num`/`args` must match a valid entry in the version-1 syscall table.
unsafe fn syscall(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") TABLE_VERSION,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

// Thin wrappers over the raw syscall.
fn invoke(desc_ptr: u64, reply_ptr: u64, reply_cap: u64) -> u64 {
    unsafe { syscall(SYS_INVOKE, desc_ptr, reply_ptr, reply_cap) }
}

fn contract_id(name_ptr: u64) -> u64 {
    unsafe { syscall(SYS_CONTRACT_ID, name_ptr, 0, 0) }
}

fn cap_dup(id: u64) -> u64 {
    unsafe { syscall(SYS_CAP_DUP, id, 0, 0) }
}

fn cap_query(id: u64, name_ptr: u64) -> u64 {
    unsafe { syscall(SYS_CAP_QUERY, id, name_ptr, 0) }
}

/// Not exercised by this demo; kept for the P6 delegation story.
#[allow(dead_code)]
fn cap_delegate(id: u64, target: u64) -> u64 {
    unsafe { syscall(SYS_CAP_DELEGATE, id, target, 0) }
}

/// One marshalled invoke argument.
enum DescArg<'a> {
    U64(u64),
    Buf(&'a [u8]),
    Str(&'a [u8]),
}

/// Build an invoke descriptor; returns its length in bytes.
fn build_desc(desc: &mut [u8], cap: u64, cid: u64, hook: u64, args: &[DescArg]) -> usize {
    let mut c = 0;
    push_u64(desc, &mut c, cap);
    push_u64(desc, &mut c, cid);
    push_u64(desc, &mut c, hook);
    push_u64(desc, &mut c, args.len() as u64);
    for a in args {
        match a {
            DescArg::U64(v) => {
                push_u64(desc, &mut c, TAG_U64);
                push_u64(desc, &mut c, *v);
            }
            DescArg::Buf(b) => {
                push_u64(desc, &mut c, TAG_BUF);
                push_u64(desc, &mut c, b.len() as u64);
                push_bytes(desc, &mut c, b);
            }
            DescArg::Str(s) => {
                push_u64(desc, &mut c, TAG_STR);
                push_u64(desc, &mut c, s.len() as u64);
                push_bytes(desc, &mut c, s);
            }
        }
    }
    c
}

fn push_u64(buf: &mut [u8], cursor: &mut usize, v: u64) {
    buf[*cursor..*cursor + 8].copy_from_slice(&v.to_le_bytes());
    *cursor += 8;
}

fn push_bytes(buf: &mut [u8], cursor: &mut usize, b: &[u8]) {
    buf[*cursor..*cursor + b.len()].copy_from_slice(b);
    *cursor += b.len();
}

fn read_u64(reply: &[u8], cursor: &mut usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&reply[*cursor..*cursor + 8]);
    *cursor += 8;
    u64::from_le_bytes(b)
}

/// Run an invoke and validate the reply header; returns (nvalues, ncaps, cursor).
fn invoke_reply(
    desc: &mut [u8],
    reply: &mut [u8],
    reply_cap: u64,
    serial_cap: u64,
    cid_serial: u64,
) -> (u64, u64, usize) {
    let ret = invoke(desc.as_ptr() as u64, reply.as_ptr() as u64, reply_cap);
    if ret == u64::MAX {
        die(serial_cap, cid_serial, b"[init] invoke: bad descriptor/reply ptr\n");
    }
    let mut cur = 0;
    let status = read_u64(reply, &mut cur);
    if status != REPLY_OK {
        puts_cap(serial_cap, cid_serial, b"[init] invoke status: ");
        print_u64(serial_cap, cid_serial, status);
        die(serial_cap, cid_serial, b"[init] invoke failed\n");
    }
    let nvalues = read_u64(reply, &mut cur);
    let ncaps = read_u64(reply, &mut cur);
    (nvalues, ncaps, cur)
}

/// Build and run a serial puts over a capability; ignores the reply.
fn puts_cap(serial_cap: u64, cid_serial: u64, s: &[u8]) {
    let mut desc = [0u8; 512];
    let reply = [0u8; 64];
    let _ = build_desc(&mut desc, serial_cap, cid_serial, hook_id("puts"), &[DescArg::Buf(s)]);
    let _ = invoke(desc.as_ptr() as u64, reply.as_ptr() as u64, 64);
}

/// Print a decimal u64 over a serial cap, followed by a newline.
fn print_u64(cap: u64, cid: u64, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut len = 0;
    if v == 0 {
        buf[len] = b'0';
        len = 1;
    }
    while v > 0 {
        buf[len] = b'0' + (v % 10) as u8;
        v /= 10;
        len += 1;
    }
    let mut i = 0;
    while i < len / 2 {
        let t = buf[i];
        buf[i] = buf[len - 1 - i];
        buf[len - 1 - i] = t;
        i += 1;
    }
    puts_cap(cap, cid, &buf[..len]);
    puts_cap(cap, cid, b"\n");
}

/// Print raw bytes over a serial cap.
fn print_str(cap: u64, cid: u64, s: &[u8]) {
    puts_cap(cap, cid, s);
}

/// Print a failure message and exit with status 1.
fn die(serial_cap: u64, cid_serial: u64, msg: &[u8]) -> ! {
    puts_cap(serial_cap, cid_serial, msg);
    unsafe {
        syscall(SYS_EXIT, 1, 0, 0);
    }
    unreachable!()
}

/// Panic handler — ring 3 has no unwind; just halt.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe {
            asm!("pause");
        }
    }
}

// ── Freestanding mem intrinsics ──────────────────────────────────────────
//
// The custom `x86_64-init` target builds `core` from source via
// `-Zbuild-std=core`, whose `compiler_builtins` does not export the `mem`
// feature (the prebuilt core shipped for the kernel target does). `core`'s
// slice/array helpers (`copy_from_slice`, zeroing, `==`) reference these C
// symbols, so they must be provided here or the final link fails.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        for i in 0..n {
            *dest.add(i) = *src.add(i);
        }
    }
    dest
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if dest < src as *mut u8 {
            for i in 0..n {
                *dest.add(i) = *src.add(i);
            }
        } else {
            let mut i = n;
            while i > 0 {
                i -= 1;
                *dest.add(i) = *src.add(i);
            }
        }
    }
    dest
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    unsafe {
        for i in 0..n {
            *dest.add(i) = c as u8;
        }
    }
    dest
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    unsafe {
        for i in 0..n {
            let av = *a.add(i);
            let bv = *b.add(i);
            if av != bv {
                return (av as i32) - (bv as i32);
            }
        }
    }
    0
}
