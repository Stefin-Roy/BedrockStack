//! `doom` — the ring-3 DOOM port for BedrockOS.
//!
//! A GPL-2.0+ userspace game built on the vendored `doomgeneric` engine
//! (`third_party/doomgeneric`, id Software + Simon Howard GPL code) plus our
//! own platform glue (`platform/`).  It is deliberately isolated from the
//! kernel, `common`, `framebuffer`, and `user/libc` — none of which are GPL.
//!
//! Build model: `build.rs` compiles the C engine with WSL gcc into
//! `libdoomgeneric.a`; this crate links it whole-archive.  Entry is `_start`
//! from `user/libc`'s crt, which calls `entry_main` here; we build a C `argv`
//! from `/proc/self/args` and hand off to the engine's `main`.

#![no_std]
#![no_main]

use core::ffi::c_char;
use core::ptr;

// The C engine archive (built by `build.rs`).  Whole-archive so every engine
// object is included — no surprises from function-pointer tables.
#[link(name = "doomgeneric", kind = "static", modifiers = "+whole-archive")]
unsafe extern "C" {
    fn main(argc: core::ffi::c_int, argv: *const *const c_char) -> core::ffi::c_int;
}

// ── Static buffers (single game task; never concurrent) ──────────────
//
// The kernel's `write` syscall consumes the caller's buffer in place
// (`copy_user_out` with an empty response zero-fills past the output), so
// writes to drivers that still take the generic path — serial, audio — go
// through a scratch copy.  `/dev/fb` has a dedicated fast-path that leaves
// the caller's memory untouched, so no scratch is needed for the framebuffer.
// These live in .bss, not the 32 KiB user stack.

static mut FB_SCRATCH: [u8; 8192] = [0u8; 8192];
static mut AUDIO_SCRATCH: [u8; 8192] = [0u8; 8192];
const ARGS_CAP: usize = 512;
static mut ARGS_BUF: [u8; ARGS_CAP] = [0u8; ARGS_CAP];
const ARGV_PTRS_CAP: usize = 17;
static mut ARGV_PTRS: [*const c_char; ARGV_PTRS_CAP] = [ptr::null(); ARGV_PTRS_CAP];
static DOOM_ARGV0: [u8; 5] = *b"doom\0";

/// Wire layout of `:mode` — 29 bytes, packed (matches the kernel's
/// `FB_MODE` schema output byte-for-byte).
#[repr(C, packed)]
pub struct BedrockFbMode {
    pub present: u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bpp: u32,
    pub pixel_format: u32,
    pub size: u64,
}

/// Query `/dev/fb:mode`.  Fills `out` and returns 0, or -1 when no
/// framebuffer is present.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_fb_mode(out: *mut BedrockFbMode) -> i32 {
    let mut buf = [0u8; 32];
    let len = buf.len();
    // `len` = buffer size so the kernel copies the response back into `buf`;
    // `:mode`'s input is an ignored BLOB (see kernel provider/dev.rs).
    let r = unsafe { libc::syscall::write_path(b"/dev/fb:mode\0", &mut buf, len, 0) };
    if r < 29 || buf[0] == 0 {
        return -1;
    }
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), out as *mut u8, 29);
    }
    0
}

/// Write `len` bytes at byte `offset` of the scanout framebuffer.  Returns
/// total bytes written or -errno.
///
/// The whole buffer is passed in a single `write` — the kernel fast-path for
/// `/dev/fb` copies it straight into the scanout without consuming/zeroing the
/// caller's memory (and without per-chunk allocations), so no scratch is
/// needed here.  The caller's buffer is rewritten every frame regardless.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_fb_write(offset: u64, src: *const u8, len: usize) -> isize {
    if src.is_null() {
        return -1;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(src as *mut u8, len) };
    unsafe { libc::syscall::write_path(b"/dev/fb\0", buf, len, offset) }
}

/// Monotonic clock: nanoseconds since boot (`/kernel/timer`).
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_now_ns() -> u64 {
    let mut buf = [0u8; 8];
    let r = unsafe { libc::syscall::read_path(b"/kernel/timer\0", &mut buf, 0) };
    if r < 8 {
        return 0;
    }
    u64::from_le_bytes(buf)
}

/// Cooperative sleep in milliseconds (parks this task).
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_sleep_ms(ms: u64) -> isize {
    libc::process::sleep_ms(ms)
}

/// Drain `/input/events` into `buf` (cap bytes).  Wire: `u32 LE count` then
/// 24-byte entries `{timestamp u64, device u32, type u32, code u32,
/// value i32}`.  Non-blocking; returns bytes read or -errno.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_read_events(buf: *mut u8, cap: usize) -> isize {
    if buf.is_null() || cap == 0 {
        return -1;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    unsafe { libc::syscall::read_path(b"/input/events\0", slice, 0) }
}

/// Our spawn arguments from `/proc/self/args`, payload only (no length
/// prefix), NUL-free.  Returns payload length or -1.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_args(buf: *mut u8, cap: usize) -> isize {
    if buf.is_null() || cap == 0 {
        return -1;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    libc::process::args(slice)
}

/// Queue `len` bytes of interleaved i16-LE stereo PCM at 48 kHz for the
/// kernel's audio pump (`/driver/audio:play_pcm`).  Returns 0 on success or
/// -errno.  Single pushes are capped so one call cannot starve the queue.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_audio_play(src: *const u8, len: usize) -> isize {
    if src.is_null() {
        return -1;
    }
    const MAX: usize = 8188; // 4-byte length prefix + payload fits AUDIO_SCRATCH
    if len == 0 || len > MAX {
        return -1;
    }
    let base = ptr::addr_of_mut!(AUDIO_SCRATCH) as *mut u8;
    unsafe {
        ptr::copy_nonoverlapping((len as u32).to_le_bytes().as_ptr(), base, 4);
        ptr::copy_nonoverlapping(src, base.add(4), len);
        let total = 4 + len;
        let scratch = core::slice::from_raw_parts_mut(base, total);
        libc::syscall::write_path(b"/driver/audio:play_pcm\0", scratch, total, 0)
    }
}

/// Append raw bytes to COM1 (`/driver/debugserial`, Blob schema — no prefix).
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_serial(src: *const u8, len: usize) -> isize {
    if src.is_null() || len == 0 {
        return -1;
    }
    const CHUNK: usize = 8192;
    let base = ptr::addr_of_mut!(FB_SCRATCH) as *mut u8;
    let mut off = 0usize;
    while off < len {
        let n = core::cmp::min(len - off, CHUNK);
        unsafe {
            ptr::copy_nonoverlapping(src.add(off), base, n);
            let scratch = core::slice::from_raw_parts_mut(base, n);
            let r = libc::syscall::write_path(b"/driver/debugserial\0", scratch, n, 0);
            if r < 0 {
                return r;
            }
        }
        off += n;
    }
    len as isize
}

/// Terminate this process with `code` (never returns).
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_exit(code: i32) -> ! {
    libc::process::exit(code as usize)
}

/// Entry point invoked by `_start` (libc crt).  Builds a C `argv` from our
/// spawn args and hands control to the engine's `main` (which runs forever).
#[unsafe(no_mangle)]
pub extern "C" fn entry_main() -> usize {
    // The engine writes savegames and its config via relative paths (e.g.
    // `.savegame/doomsav0.dsg`) against the process CWD, which defaults to
    // `/` — the read-only unispace registry root.  Repoint the CWD at the
    // writable tmpfs (`A>`) so saves land in `/A/.savegame/`.
    let ch = libc::vfs::chdir(b"/A\0".as_ptr() as *const core::ffi::c_char);
    if ch != 0 {
        let s = b"[doom] chdir /A failed\n";
        let _ = bedrock_serial(s.as_ptr(), s.len());
    } else {
        let s = b"[doom] chdir /A ok\n";
        let _ = bedrock_serial(s.as_ptr(), s.len());
    }

    let argv0 = DOOM_ARGV0.as_ptr() as *const c_char;

    let mut argc = 1usize;
    unsafe { ARGV_PTRS[0] = argv0 };

    let nargs = unsafe {
        let base = ptr::addr_of_mut!(ARGS_BUF) as *mut u8;
        let slice = core::slice::from_raw_parts_mut(base, ARGS_CAP);
        libc::process::args(slice)
    };
    {
        let msg = b"[doom] bedrock_args len=";
        let _ = bedrock_serial(msg.as_ptr(), msg.len());
        // crude decimal
        let mut tmp = [0u8; 20];
        let mut n = 0usize;
        let mut v = if nargs < 0 { 0 } else { nargs as u64 };
        let neg = nargs < 0;
        if neg {
            tmp[n] = b'-';
            n += 1;
            if nargs == -1 {
                // -1 -> 0xffff... but print as -1
                tmp[n] = b'1';
                n += 1;
            } else {
                v = (-nargs) as u64;
                let mut digits = [0u8; 20];
                let mut d = 20usize;
                if v == 0 { digits[19]=b'0'; d=19; } else { while v>0 { d-=1; digits[d]=b'0'+(v%10) as u8; v/=10; } }
                for i in d..20 { tmp[n]=digits[i]; n+=1; }
            }
        } else {
            if v==0 { tmp[n]=b'0'; n+=1; } else {
                let mut digits=[0u8;20]; let mut d=20usize;
                while v>0 { d-=1; digits[d]=b'0'+(v%10) as u8; v/=10; }
                for i in d..20 { tmp[n]=digits[i]; n+=1; }
            }
        }
        let nl = b"\n";
        let _ = bedrock_serial(tmp.as_ptr(), n);
        let _ = bedrock_serial(nl.as_ptr(), 1);
    }
    if nargs > 0 {
        let mut n = nargs as usize;
        if n > ARGS_CAP {
            n = ARGS_CAP;
        }
        unsafe {
            let mut i = 0usize;
            loop {
                while i < n && matches!(ARGS_BUF[i], b' ' | b'\t' | b'\r' | b'\n') {
                    i += 1;
                }
                if i >= n || argc >= ARGV_PTRS_CAP - 1 {
                    break;
                }
                ARGV_PTRS[argc] = (ptr::addr_of_mut!(ARGS_BUF) as *mut u8).add(i) as *const c_char;
                argc += 1;
                while i < n && !matches!(ARGS_BUF[i], b' ' | b'\t' | b'\r' | b'\n') {
                    i += 1;
                }
                if i < n {
                    ARGS_BUF[i] = 0; // NUL-terminate this token
                }
            }
        }
    }
    // argv is null-terminated because ARGV_PTRS is zero-initialised.
    let argv = ptr::addr_of!(ARGV_PTRS) as *const *const c_char;
    let code = unsafe { main(argc as core::ffi::c_int, argv) };
    code as usize
}
