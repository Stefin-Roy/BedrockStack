//! BedrockOS first userspace program (`INIT`), built on the `libc` crate.
//!
//! Writes everything to `/proc/self/std/out`; the kernel boot path reads it
//! back and prints it to serial after INIT exits (see `task/load.rs`). The
//! supervisor (no args) runs the demo, spawns a copy of itself as a child
//! (args = "child"), waits for it, and echoes the child's captured stdout —
//! proving the per-process std streams end to end. Then it paints the
//! framebuffer, plays the startup chime (`\EFI\BEDROCK\STARTUP.WAV`) through
//! the kernel audio device, and on any keypress launches DOOM
//! (`/B/EFI/BEDROCK/DOOM`) with the Freedoom IWAD, repainting when the game
//! exits.

#![no_std]
#![no_main]

/// Paint a vertical RGB gradient across the whole framebuffer.
/// Paint a 2D RGB gradient (red spans x, green spans y) over the visible
/// framebuffer region, leaving stride padding untouched.
///
/// Safe for any `bpp`: only the channels that exist are written, each chunk is
/// zero-filled first so bytes for `bpp > 4` cannot carry stale data into the
/// next pixel, and `write_at` either performs the whole write or returns an
/// error (it never does a partial write).
fn paint_gradient(mode: &libc::fb::FbMode) {
    let width = mode.width as usize;
    let height = mode.height as usize;
    let stride = mode.stride as usize;
    let bpp = mode.bpp as usize;
    let rowbytes = stride.saturating_mul(bpp);
    if width == 0 || height == 0 || stride == 0 || bpp == 0 || rowbytes == 0 {
        return;
    }
    // Fixed stack row buffer, chunked so each write fits.  If the buffer can't
    // even hold one pixel (bpp > ROW_BUF_BYTES) there is nothing we can paint.
    const ROW_BUF_BYTES: usize = 2560;
    let pixels_per_chunk = ROW_BUF_BYTES / bpp;
    if pixels_per_chunk == 0 {
        return;
    }
    // Gradient spans exactly 0..=255 across each axis (denominator = max index).
    let r_den = width.saturating_sub(1).max(1);
    let g_den = height.saturating_sub(1).max(1);
    let mut row = [0u8; ROW_BUF_BYTES];
    for y in 0..height {
        let mut x = 0usize;
        while x < width {
            let count = (width - x).min(pixels_per_chunk);
            let bytes = count * bpp;
            row[..bytes].fill(0);
            for i in 0..count {
                let px = x + i;
                let r = ((px * 255) / r_den) as u8;
                let g = ((y * 255) / g_den) as u8;
                let b = 64u8;
                let a = 255u8;
                let off = i * bpp;
                row[off] = match mode.pixel_format {
                    1 => r,
                    _ => b,
                };
                if bpp >= 2 {
                    row[off + 1] = g;
                }
                if bpp >= 3 {
                    row[off + 2] = match mode.pixel_format {
                        1 => b,
                        _ => r,
                    };
                }
                if bpp >= 4 {
                    row[off + 3] = a;
                }
            }
            let byte_off = (y * rowbytes + x * bpp) as u64;
            let rw = libc::fb::write_at(byte_off, &row[..bytes]);
            if rw < 0 {
                return;
            }
            x += count;
        }
    }
}

/// Poll `/input/events` (non-blocking) and report whether a key was pressed.
/// Wire: `u32 LE count` then `count` 24-byte entries
/// `{ts:u64, device:u32, type:u32, code:u32, value:i32}` (all LE); a key
/// press has `type == 1 && value != 0`.
fn key_pressed() -> bool {
    let mut buf = [0u8; 1024];
    let r = unsafe { libc::syscall::read_path(b"/input/events\0", &mut buf, 0) };
    if r < 0 {
        serial_puts(b"[INIT] input read ERROR r=");
        serial_put_u64_hex(r as u64);
        serial_puts(b"\n");
        return false;
    }
    if r < 4 {
        return false;
    }
    let avail = (r as usize).min(buf.len());
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // Never trust the count before bounding it by the bytes actually read.
    if n > (avail - 4) / 24 {
        return false;
    }
    let mut i = 4usize;
    let mut pressed = false;
    while i < 4 + n * 24 {
        let etype = u32::from_le_bytes([buf[i + 12], buf[i + 13], buf[i + 14], buf[i + 15]]);
        let value = i32::from_le_bytes([buf[i + 20], buf[i + 21], buf[i + 22], buf[i + 23]]);
        if etype == 1 && value != 0 {
            pressed = true;
        }
        i += 24;
    }
    if n > 0 {
        serial_puts(b"[INIT] input read n=");
        serial_put_u64_hex(n as u64);
        serial_puts(b" pressed=");
        serial_put_u64_hex(pressed as u64);
        serial_puts(b"\n");
    }
    pressed
}

/// Spin until the next key press.
fn wait_for_key() {
    loop {
        if key_pressed() {
            return;
        }
        libc::process::sleep_ms(20);
    }
}

/// Dump a finished child's captured stdout to ours (diagnostic aid).
fn drain_child_stdout(pid: isize) {
    let mut spath = [0u8; 40];
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
    // Keep the large drain buffer in `.bss`; the user stack is only 32 KiB.
    static mut CHILD_STDOUT_SCRATCH: [u8; libc::IO_CHUNK_BYTES] = [0; libc::IO_CHUNK_BYTES];
    let sbuf = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(CHILD_STDOUT_SCRATCH) as *mut u8,
            libc::IO_CHUNK_BYTES,
        )
    };
    loop {
        let sr = unsafe { libc::syscall::read_path(&spath[..slen], sbuf, 0) };
        if sr <= 0 {
            break;
        }
        unsafe {
            libc::stdio::write(1, sbuf.as_ptr() as *const core::ffi::c_void, sr as usize);
        }
    }
}

/// Play the startup chime from the ESP. The asset is RIFF/WAVE PCM 48 kHz
/// stereo 16-bit — the kernel's native `:play_pcm` format — so we just parse
/// the header for the `data` offset and stream the payload in 20 ms chunks
/// as `[u32 LE byte_len][i16 LE samples]`. The kernel's 8-slot queue parks us
/// when full, which paces the stream; if audio is absent every write just
/// errors and we exit silently.
fn play_startup_wav() {
    const CHUNK_BYTES: usize = 3840; // 20 ms of 48 kHz stereo 16-bit
    serial_puts(b"[wav] fopen\n");
    let f = libc::stdio::fopen(c"/B/EFI/BEDROCK/STARTUP.WAV".as_ptr(), c"r".as_ptr());
    if f.is_null() {
        serial_puts(b"[wav] fopen failed\n");
        return;
    }
    serial_puts(b"[wav] open ok\n");
    let mut hdr = [0u8; 64];
    let n = libc::stdio::fread(hdr.as_mut_ptr() as *mut core::ffi::c_void, 1, hdr.len(), f);
    if n < 12 || &hdr[..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        libc::stdio::fclose(f);
        return;
    }
    let mut data_off = 0usize;
    let mut i = 12usize;
    while i + 8 <= n as usize {
        if &hdr[i..i + 4] == b"data" {
            data_off = i + 8;
            break;
        }
        let sz = u32::from_le_bytes([hdr[i + 4], hdr[i + 5], hdr[i + 6], hdr[i + 7]]) as usize;
        i += 8 + sz + (sz & 1);
        if i >= n as usize {
            break;
        }
    }
    if data_off == 0 {
        libc::stdio::fclose(f);
        return;
    }
    let _ = libc::stdio::fseek(f, data_off as core::ffi::c_long, 0); // SEEK_SET
    let mut wire = [0u8; 4 + CHUNK_BYTES];
    let mut iter = 0usize;
    loop {
        let got = libc::stdio::fread(
            wire[4..].as_mut_ptr() as *mut core::ffi::c_void,
            1,
            CHUNK_BYTES,
            f,
        );
        if got == 0 {
            break;
        }
        if iter < 16 {
            serial_puts(b"[wav] got ");
            serial_put_u64_hex(got as u64);
            serial_puts(b"\n");
        }
        iter += 1;
        let g = got as u32;
        wire[0] = (g & 0xFF) as u8;
        wire[1] = ((g >> 8) & 0xFF) as u8;
        wire[2] = ((g >> 16) & 0xFF) as u8;
        wire[3] = ((g >> 24) & 0xFF) as u8;
        let _ = unsafe {
            libc::syscall::write_path(
                b"/driver/audio:play_pcm\0",
                &mut wire[..4 + got],
                4 + got,
                0,
            )
        };
        if iter < 16 {
            serial_puts(b"[wav] wrote\n");
        }
        if got < CHUNK_BYTES {
            break;
        }
    }
    serial_puts(b"[wav] done\n");
    libc::stdio::fclose(f);
}

/// Write a diagnostic line to live serial (COM1) via `/driver/debugserial`.
/// INIT's stdout is captured (only dumped after exit), so this is the way to
/// see progress on the boot console immediately.
fn serial_puts(s: &[u8]) {
    let mut buf = [0u8; 128];
    let n = s.len().min(buf.len());
    buf[..n].copy_from_slice(&s[..n]);
    unsafe {
        let _ = libc::syscall::write_path(b"/driver/debugserial\0", &mut buf[..n], n, 0);
    }
}

/// Append a 64-bit value as lowercase hex to a `&[u8]` diagnostic string.
fn serial_put_u64_hex(mut v: u64) {
    let mut buf = [0u8; 16];
    let mut i = 16;
    loop {
        i -= 1;
        let d = (v & 0xF) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    serial_puts(&buf[i..]);
}

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
        libc::stdio::printf(
            c"hello from user space (pid=%d)\n".as_ptr(),
            libc::process::getpid(),
        );
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

    // 4. Paint a vertical RGB gradient into /dev/fb if a mode is advertised.
    //    No fb mode is not an error: INIT carries on without painting.
    let Some(mode) = libc::fb::query_mode() else {
        libc::stdio::puts(c"fb: no /dev/fb mode".as_ptr());
        return 0;
    };
    let width = mode.width as usize;
    let height = mode.height as usize;
    unsafe {
        libc::stdio::printf(
            c"fb: %ux%u stride %u bpp %u\n".as_ptr(),
            width as core::ffi::c_uint,
            height as core::ffi::c_uint,
            mode.stride as core::ffi::c_uint,
            mode.bpp as core::ffi::c_uint,
        );
    }

    // 5. Boot is finished: play the startup chime, then stay alive —
    //    repaint the gradient, and on any keypress launch DOOM with the
    //    Freedoom IWAD, wait for it, and dump its stdout. INIT is the parent
    //    task, so it must not exit while the OS keeps running.
    serial_puts(b"[INIT] PLAYING SOUND FUNCTION\n");
    play_startup_wav();
    loop {
        serial_puts(b"[INIT] reached paint loop\n");
        paint_gradient(&mode);
        unsafe {
            libc::stdio::puts(c"fb: press any key to launch DOOM".as_ptr());
        }
        // DIAGNOSTIC: keyboard delivery stalls after the boot path, so skip
        // the keypress and auto-launch DOOM to test the hardcoded -iwad path.
        // wait_for_key();
        serial_puts(b"[INIT] auto-jumping to DOOM\n");
        let pid = libc::process::spawn("/B/EFI/BEDROCK/DOOM", "-iwad /B/EFI/BEDROCK/FREEDOOM.WAD");
        if pid < 0 {
            unsafe {
                libc::stdio::printf(c"DOOM spawn failed (%d)\n".as_ptr(), pid);
            }
            continue;
        }
        unsafe {
            libc::stdio::printf(c"DOOM pid=%d\n".as_ptr(), pid);
        }
        let code = libc::process::wait(pid as u64);
        unsafe {
            libc::stdio::printf(c"DOOM exited code=%d\n".as_ptr(), code);
        }
        drain_child_stdout(pid);
    }
}
