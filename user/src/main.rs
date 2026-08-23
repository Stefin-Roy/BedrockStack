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

extern crate alloc;

use core::alloc::Layout;

const R: u8 = 1;
const RW: u8 = 3;

fn child_caps() -> &'static [libc::process::Cap<'static>] {
    &[
        libc::process::Cap { path: "proc", method: None, perm: RW },
        libc::process::Cap { path: "proc/self", method: None, perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("exit"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("brk"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("mmap"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("munmap"), perm: RW },
        libc::process::Cap { path: "proc/self/args", method: None, perm: R },
        libc::process::Cap { path: "proc/self/caps", method: None, perm: R },
        libc::process::Cap { path: "proc/self/std", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/out", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/err", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/out", method: Some("get"), perm: R },
    ]
}

fn posixcheck_caps() -> &'static [libc::process::Cap<'static>] {
    &[
        // VFS /A for file ops
        libc::process::Cap { path: "A", method: None, perm: RW },
        libc::process::Cap { path: "A", method: Some("create"), perm: RW },
        libc::process::Cap { path: "A", method: Some("mkdir"), perm: RW },
        libc::process::Cap { path: "A", method: Some("rmdir"), perm: RW },
        libc::process::Cap { path: "A", method: Some("unlink"), perm: RW },
        libc::process::Cap { path: "A", method: Some("rename"), perm: RW },
        libc::process::Cap { path: "A", method: Some("symlink"), perm: RW },
        libc::process::Cap { path: "A", method: Some("link"), perm: RW },
        libc::process::Cap { path: "A", method: Some("mkfifo"), perm: RW },
        libc::process::Cap { path: "A", method: Some("mknod"), perm: RW },
        libc::process::Cap { path: "A", method: Some("stat"), perm: RW },
        // B/EFI/BEDROCK/POSIXCHECK
        libc::process::Cap { path: "B", method: None, perm: R },
        libc::process::Cap { path: "B/EFI", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK/POSIXCHECK", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK/POSIXCHECK", method: Some("stat"), perm: R },
        // sys for uname/sysconf
        libc::process::Cap { path: "sys", method: None, perm: R },
        libc::process::Cap { path: "sys/version", method: None, perm: R },
        libc::process::Cap { path: "sys/cpus", method: None, perm: R },
        // kernel timer
        libc::process::Cap { path: "kernel", method: None, perm: R },
        libc::process::Cap { path: "kernel/timer", method: None, perm: R },
        libc::process::Cap { path: "kernel/timer", method: Some("sleep"), perm: RW },
        libc::process::Cap { path: "kernel/timer", method: Some("sleep_ms"), perm: RW },
        libc::process::Cap { path: "kernel/timer", method: Some("until"), perm: RW },
        libc::process::Cap { path: "kernel/timer", method: Some("epoch_secs"), perm: RW },
        // proc
        libc::process::Cap { path: "proc", method: None, perm: RW },
        libc::process::Cap { path: "proc/self", method: None, perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("exit"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("spawn_caps"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("brk"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("mmap"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("munmap"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("wait"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("yield"), perm: RW },
        libc::process::Cap { path: "proc/self/status", method: None, perm: R },
        libc::process::Cap { path: "proc/self/args", method: None, perm: R },
        libc::process::Cap { path: "proc/self/caps", method: None, perm: R },
        libc::process::Cap { path: "proc/self/std", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/in", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/out", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/err", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/out", method: Some("get"), perm: R },
        libc::process::Cap { path: "proc/self/std/err", method: Some("get"), perm: R },
        // file methods for /A files (auto-granted but pre-grant for stat)
        libc::process::Cap { path: "A", method: Some("truncate"), perm: RW },
    ]
}

fn doom_caps() -> &'static [libc::process::Cap<'static>] {
    &[
        // VFS
        libc::process::Cap { path: "A", method: None, perm: RW },
        libc::process::Cap { path: "A", method: Some("create"), perm: RW },
        libc::process::Cap { path: "A", method: Some("mkdir"), perm: RW },
        libc::process::Cap { path: "A", method: Some("rmdir"), perm: RW },
        libc::process::Cap { path: "A", method: Some("unlink"), perm: RW },
        libc::process::Cap { path: "A", method: Some("rename"), perm: RW },
        libc::process::Cap { path: "A", method: Some("symlink"), perm: RW },
        libc::process::Cap { path: "A", method: Some("link"), perm: RW },
        libc::process::Cap { path: "A", method: Some("mkfifo"), perm: RW },
        libc::process::Cap { path: "A", method: Some("mknod"), perm: RW },
        libc::process::Cap { path: "A", method: Some("stat"), perm: RW },
        libc::process::Cap { path: "B", method: None, perm: R },
        libc::process::Cap { path: "B/EFI", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK/DOOM", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK/DOOM", method: Some("stat"), perm: RW },
        libc::process::Cap { path: "B/EFI/BEDROCK/FREEDOOM.WAD", method: None, perm: R },
        libc::process::Cap { path: "B/EFI/BEDROCK/FREEDOOM.WAD", method: Some("stat"), perm: RW },
        // devices
        libc::process::Cap { path: "dev", method: None, perm: R },
        libc::process::Cap { path: "dev/fb", method: None, perm: RW },
        libc::process::Cap { path: "dev/fb", method: Some("mode"), perm: RW },
        libc::process::Cap { path: "dev/fb", method: Some("clear"), perm: RW },
        libc::process::Cap { path: "driver", method: None, perm: R },
        libc::process::Cap { path: "driver/debugserial", method: None, perm: RW },
        libc::process::Cap { path: "driver/audio", method: None, perm: RW },
        libc::process::Cap { path: "driver/audio", method: Some("play_pcm"), perm: RW },
        libc::process::Cap { path: "driver/audio", method: Some("play_tone"), perm: RW },
        libc::process::Cap { path: "input", method: None, perm: R },
        libc::process::Cap { path: "input/events", method: None, perm: R },
        libc::process::Cap { path: "kernel", method: None, perm: R },
        libc::process::Cap { path: "kernel/timer", method: None, perm: R },
        libc::process::Cap { path: "kernel/timer", method: Some("sleep"), perm: RW },
        libc::process::Cap { path: "kernel/timer", method: Some("sleep_ms"), perm: RW },
        libc::process::Cap { path: "kernel/timer", method: Some("until"), perm: RW },
        libc::process::Cap { path: "kernel/timer", method: Some("epoch_secs"), perm: RW },
        // proc
        libc::process::Cap { path: "proc", method: None, perm: RW },
        libc::process::Cap { path: "proc/self", method: None, perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("exit"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("brk"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("mmap"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("munmap"), perm: RW },
        libc::process::Cap { path: "proc/self", method: Some("yield"), perm: RW },
        libc::process::Cap { path: "proc/self/status", method: None, perm: R },
        libc::process::Cap { path: "proc/self/args", method: None, perm: R },
        libc::process::Cap { path: "proc/self/caps", method: None, perm: R },
        libc::process::Cap { path: "proc/self/std", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/in", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/out", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/err", method: None, perm: RW },
        libc::process::Cap { path: "proc/self/std/out", method: Some("get"), perm: R },
        libc::process::Cap { path: "proc/self/std/err", method: Some("get"), perm: R },
        libc::process::Cap { path: "proc/self/std/in", method: Some("get"), perm: R },
    ]
}

/// Paint a vertical RGB gradient across the whole framebuffer.
/// Paint a 2D RGB gradient (red spans x, green spans y) over the visible
/// framebuffer region, leaving stride padding untouched.
///
/// Optimised without losing correctness: LUTs remove per-pixel divisions
/// (w*h*2 -> w+h), branches hoisted, 8 KiB stack row buffer (one syscall/
/// row for 1920x4) with direct write_path fast-path, and a heap bulk attempt
/// (one syscall/frame) for stride==width that falls back to the stack path
/// on OOM/short-write so the screen never stays black.
fn paint_gradient(mode: &libc::fb::FbMode) {
    let width = mode.width as usize;
    let height = mode.height as usize;
    let stride = mode.stride as usize;
    let bpp = mode.bpp as usize;
    let rowbytes = match stride.checked_mul(bpp) {
        Some(v) if v != 0 => v,
        _ => return,
    };
    if width == 0 || height == 0 || stride == 0 || bpp == 0 || width > stride {
        return;
    }
    let r_den = width.saturating_sub(1).max(1);
    let g_den = height.saturating_sub(1).max(1);
    let is_rgb = mode.pixel_format == 1;

    // Stack LUTs: 4096 covers every QEMU GOP mode; fallback to division if larger.
    const LUT_CAP: usize = 4096;
    let mut r_lut = [0u8; LUT_CAP];
    let mut g_lut = [0u8; LUT_CAP];
    let use_r_lut = width <= LUT_CAP;
    let use_g_lut = height <= LUT_CAP;
    if use_r_lut {
        for x in 0..width {
            r_lut[x] = ((x * 255) / r_den) as u8;
        }
    }
    if use_g_lut {
        for y in 0..height {
            g_lut[y] = ((y * 255) / g_den) as u8;
        }
    }

    // Heap bulk fast-path: padded total <=16 MiB, LUT-covered.
    // One heap buffer (rowbytes*height, includes stride padding) + one
    // write_path, falls through on any failure so screen never stays black.
    const MAX_COPY: usize = 16 * 1024 * 1024;
    if use_r_lut && use_g_lut {
        if let Some(total) = rowbytes.checked_mul(height) {
            if total != 0 && total <= MAX_COPY {
                let align = if bpp == 4 { 4 } else { 1 };
                if let Ok(layout) = Layout::from_size_align(total, align) {
                    let ptr = unsafe { alloc::alloc::alloc(layout) };
                    if !ptr.is_null() {
                        unsafe { core::ptr::write_bytes(ptr, 0, total) };
                        if bpp == 4 {
                            if is_rgb {
                                for y in 0..height {
                                    let g = g_lut[y] as u32;
                                    let row_base = y * rowbytes;
                                    for x in 0..width {
                                        let r = r_lut[x] as u32;
                                        let px: u32 = (0xFF << 24) | (64 << 16) | (g << 8) | r;
                                        unsafe { *(ptr.add(row_base + x * 4) as *mut u32) = px; }
                                    }
                                }
                            } else {
                                for y in 0..height {
                                    let g = g_lut[y] as u32;
                                    let row_base = y * rowbytes;
                                    for x in 0..width {
                                        let r = r_lut[x] as u32;
                                        let px: u32 = (0xFF << 24) | (r << 16) | (g << 8) | 64;
                                        unsafe { *(ptr.add(row_base + x * 4) as *mut u32) = px; }
                                    }
                                }
                            }
                        } else if is_rgb {
                            for y in 0..height {
                                let g = g_lut[y];
                                let row_base = y * rowbytes;
                                for x in 0..width {
                                    let r = r_lut[x];
                                    let off = row_base + x * bpp;
                                    unsafe {
                                        *ptr.add(off) = r;
                                        if bpp >= 2 { *ptr.add(off + 1) = g; }
                                        if bpp >= 3 { *ptr.add(off + 2) = 64; }
                                        if bpp >= 4 { *ptr.add(off + 3) = 255; }
                                    }
                                }
                            }
                        } else {
                            for y in 0..height {
                                let g = g_lut[y];
                                let row_base = y * rowbytes;
                                for x in 0..width {
                                    let r = r_lut[x];
                                    let off = row_base + x * bpp;
                                    unsafe {
                                        *ptr.add(off) = 64;
                                        if bpp >= 2 { *ptr.add(off + 1) = g; }
                                        if bpp >= 3 { *ptr.add(off + 2) = r; }
                                        if bpp >= 4 { *ptr.add(off + 3) = 255; }
                                    }
                                }
                            }
                        }
                        let buf = unsafe { core::slice::from_raw_parts_mut(ptr, total) };
                        let r = unsafe { libc::syscall::write_path(b"/dev/fb\0", buf, total, 0) };
                        unsafe { alloc::alloc::dealloc(ptr, layout) };
                        if r >= 0 && (r as usize) == total {
                            return;
                        }
                    }
                }
            }
        }
    }

    // Stack per-row fallback: 8 KiB row, one fast-path write per chunk.
    const ROW_BUF_BYTES: usize = 8192;
    if bpp > ROW_BUF_BYTES {
        return;
    }
    let pixels_per_chunk = ROW_BUF_BYTES / bpp;
    if pixels_per_chunk == 0 {
        return;
    }
    let mut row = [0u8; ROW_BUF_BYTES];
    for y in 0..height {
        let g = if use_g_lut { g_lut[y] } else { ((y * 255) / g_den) as u8 };
        let mut x = 0usize;
        while x < width {
            let count = (width - x).min(pixels_per_chunk);
            let bytes = count * bpp;
            row[..bytes].fill(0);
            if is_rgb {
                match bpp {
                    4 => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i] as u32;
                                let gg = g as u32;
                                let px: u32 = (0xFF << 24) | (64 << 16) | (gg << 8) | r;
                                unsafe { *(row.as_mut_ptr().add(i * 4) as *mut u32) = px; }
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u32;
                                let gg = g as u32;
                                let px: u32 = (0xFF << 24) | (64 << 16) | (gg << 8) | r;
                                unsafe { *(row.as_mut_ptr().add(i * 4) as *mut u32) = px; }
                            }
                        }
                    }
                    3 => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i];
                                let off = i * 3;
                                row[off] = r;
                                row[off + 1] = g;
                                row[off + 2] = 64;
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u8;
                                let off = i * 3;
                                row[off] = r;
                                row[off + 1] = g;
                                row[off + 2] = 64;
                            }
                        }
                    }
                    2 => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i];
                                let off = i * 2;
                                row[off] = r;
                                row[off + 1] = g;
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u8;
                                let off = i * 2;
                                row[off] = r;
                                row[off + 1] = g;
                            }
                        }
                    }
                    1 => {
                        if use_r_lut {
                            for i in 0..count {
                                row[i] = r_lut[x + i];
                            }
                        } else {
                            for i in 0..count {
                                row[i] = ((x + i) * 255 / r_den) as u8;
                            }
                        }
                    }
                    _ => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i];
                                let off = i * bpp;
                                row[off] = r;
                                if bpp >= 2 { row[off + 1] = g; }
                                if bpp >= 3 { row[off + 2] = 64; }
                                if bpp >= 4 { row[off + 3] = 255; }
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u8;
                                let off = i * bpp;
                                row[off] = r;
                                if bpp >= 2 { row[off + 1] = g; }
                                if bpp >= 3 { row[off + 2] = 64; }
                                if bpp >= 4 { row[off + 3] = 255; }
                            }
                        }
                    }
                }
            } else {
                match bpp {
                    4 => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i] as u32;
                                let gg = g as u32;
                                let px: u32 = (0xFF << 24) | (r << 16) | (gg << 8) | 64;
                                unsafe { *(row.as_mut_ptr().add(i * 4) as *mut u32) = px; }
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u32;
                                let gg = g as u32;
                                let px: u32 = (0xFF << 24) | (r << 16) | (gg << 8) | 64;
                                unsafe { *(row.as_mut_ptr().add(i * 4) as *mut u32) = px; }
                            }
                        }
                    }
                    3 => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i];
                                let off = i * 3;
                                row[off] = 64;
                                row[off + 1] = g;
                                row[off + 2] = r;
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u8;
                                let off = i * 3;
                                row[off] = 64;
                                row[off + 1] = g;
                                row[off + 2] = r;
                            }
                        }
                    }
                    2 => {
                        for i in 0..count {
                            let off = i * 2;
                            row[off] = 64;
                            row[off + 1] = g;
                        }
                    }
                    1 => {
                        for i in 0..count {
                            row[i] = 64;
                        }
                    }
                    _ => {
                        if use_r_lut {
                            for i in 0..count {
                                let r = r_lut[x + i];
                                let off = i * bpp;
                                row[off] = 64;
                                if bpp >= 2 { row[off + 1] = g; }
                                if bpp >= 3 { row[off + 2] = r; }
                                if bpp >= 4 { row[off + 3] = 255; }
                            }
                        } else {
                            for i in 0..count {
                                let r = ((x + i) * 255 / r_den) as u8;
                                let off = i * bpp;
                                row[off] = 64;
                                if bpp >= 2 { row[off + 1] = g; }
                                if bpp >= 3 { row[off + 2] = r; }
                                if bpp >= 4 { row[off + 3] = 255; }
                            }
                        }
                    }
                }
            }
            let byte_off = (y * rowbytes + x * bpp) as u64;
            let buf = &mut row[..bytes];
            let r = unsafe { libc::syscall::write_path(b"/dev/fb\0", buf, bytes, byte_off) };
            if r < 0 {
                return;
            }
            x += count;
        }
    }
}

/// Dump a finished child's captured stdout to ours (diagnostic aid).
fn drain_child_stdout(pid: isize) {
    drain_child_stream(pid, b"/std/out\0", b"[child stdout]\n");
    drain_child_stream(pid, b"/std/err\0", b"[child stderr]\n");
}

fn drain_child_stream(pid: isize, suffix: &[u8], header: &[u8]) {
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
    for &b in suffix {
        spath[slen] = b;
        slen += 1;
    }
    // Keep the large drain buffer in `.bss`; the user stack is only 32 KiB.
    static mut CHILD_DRAIN_SCRATCH: [u8; libc::IO_CHUNK_BYTES] = [0; libc::IO_CHUNK_BYTES];
    let sbuf = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(CHILD_DRAIN_SCRATCH) as *mut u8,
            libc::IO_CHUNK_BYTES,
        )
    };
    let mut first = true;
    let mut total: usize = 0;
    loop {
        let sr = unsafe { libc::syscall::read_path(&spath[..slen], sbuf, 0) };
        if sr < 0 {
            // Surface cap/ENOENT errors so missing proc/<pid> caps are visible
            if total == 0 {
                serial_puts(b"[drain] read ");
                serial_write(suffix);
                serial_puts(b" pid=");
                serial_put_u64_hex(pid as u64);
                serial_puts(b" err=");
                // sr is negative errno like -2, -13 etc.
                let code = (-sr) as u64;
                serial_put_u64_hex(code);
                serial_puts(b"\n");
            }
            break;
        }
        if sr == 0 {
            break;
        }
        if first {
            serial_write(header);
            first = false;
        }
        total += sr as usize;
        serial_write(&sbuf[..sr as usize]);
    }
    if total > 0 {
        serial_puts(b"[drain] done ");
        serial_write(suffix);
        serial_puts(b" total=");
        serial_put_u64_hex(total as u64);
        serial_puts(b"\n");
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

/// Relay an arbitrary-length buffer to live serial in small chunks.
fn serial_write(s: &[u8]) {
    let mut buf = [0u8; 128];
    let mut off = 0usize;
    while off < s.len() {
        let n = (s.len() - off).min(buf.len());
        buf[..n].copy_from_slice(&s[off..off + n]);
        unsafe {
            let _ = libc::syscall::write_path(b"/driver/debugserial\0", &mut buf[..n], n, 0);
        }
        off += n;
    }
}

/// Serial-print a `prefix <decimal> suffix` line (e.g. an exit code report).
fn serial_puts_dec(prefix: &[u8], v: u64, suffix: &[u8]) {
    let mut buf = [0u8; 128];
    let plen = prefix.len().min(buf.len());
    buf[..plen].copy_from_slice(&prefix[..plen]);
    let mut n = plen;
    let mut digits = [0u8; 20];
    let mut d = 20usize;
    if v == 0 {
        digits[19] = b'0';
        d = 19;
    }
    let mut vv = v;
    while vv > 0 {
        d -= 1;
        digits[d] = b'0' + (vv % 10) as u8;
        vv /= 10;
    }
    for i in d..20 {
        if n < buf.len() {
            buf[n] = digits[i];
            n += 1;
        }
    }
    let m = suffix.len().min(buf.len() - n);
    buf[n..n + m].copy_from_slice(&suffix[..m]);
    n += m;
    serial_puts(&buf[..n]);
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
    serial_puts(b"[INIT] write/read ok, before spawn\n");

    // 2. Spawn a copy of ourselves as a child (args="child"), wait for it, and
    //    echo its captured stdout. The child's std/out stream survives as long
    //    as its /proc dir, so it is readable after :wait until the idle reaper
    //    runs. Spawn is now capability-checked — explicit subset required.
    serial_puts(b"[INIT] spawning child\n");
    let pid = libc::process::spawn("/B/EFI/BEDROCK/INIT", "child", child_caps());
    serial_puts(b"[INIT] spawn returned\n");
    serial_puts_dec(b"[INIT] spawn pid=", pid as u64, b"\n");
    if pid < 0 {
        libc::stdio::puts(c"spawn failed".as_ptr());
        serial_puts(b"[INIT] spawn failed path\n");
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
    // 5a. First run the POSIX conformance harness and report its outcome.
    serial_puts(b"[INIT] running POSIXCHECK\n");
    let pcheck = libc::process::spawn("/B/EFI/BEDROCK/POSIXCHECK", "", posixcheck_caps());
    if pcheck < 0 {
        serial_puts_dec(b"[INIT] POSIXCHECK spawn failed rc=", pcheck as u64, b"\n");
    } else {
        let code = libc::process::wait(pcheck as u64);
        drain_child_stdout(pcheck);
        serial_puts_dec(b"[INIT] POSIXCHECK exit code=", code as u64, b"\n");
    }
    serial_puts(b"[INIT] PLAYING SOUND FUNCTION\n");
    play_startup_wav();
    loop {
        serial_puts(b"[INIT] reached paint loop\n");
        paint_gradient(&mode);
        libc::stdio::puts(c"fb: press any key to launch DOOM".as_ptr());
        // DIAGNOSTIC: keyboard delivery stalls after the boot path, so skip
        // the keypress and auto-launch DOOM to test the hardcoded -iwad path.
        // wait_for_key();
        serial_puts(b"[INIT] auto-jumping to DOOM\n");
        let pid = libc::process::spawn("/B/EFI/BEDROCK/DOOM", "-iwad /B/EFI/BEDROCK/FREEDOOM.WAD", doom_caps());
        if pid < 0 {
            serial_puts_dec(b"[INIT] DOOM spawn failed rc=", pid as u64, b"\n");
            unsafe {
                libc::stdio::printf(c"DOOM spawn failed (%d)\n".as_ptr(), pid);
            }
            // Back off so the paint loop doesn't hot-spin on persistent failure
            libc::process::sleep_ms(200);
            continue;
        }
        serial_puts_dec(b"[INIT] DOOM spawned pid=", pid as u64, b"\n");
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
