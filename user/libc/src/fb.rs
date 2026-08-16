use crate::syscall::write_path;

/// Static mode information for the kernel `/dev/fb` framebuffer device.
#[derive(Clone, Copy, Debug)]
pub struct FbMode {
    pub present: bool,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bpp: u32,
    pub pixel_format: u32,
    pub size: u64,
}

fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// Query the framebuffer mode via the `/dev/fb:mode` method. Returns `None`
/// when no mode is advertised or the reply is too short.
pub fn query_mode() -> Option<FbMode> {
    let mut buf = [0u8; 32];
    let len = buf.len();
    // `len` = buffer size so the kernel copies the response back into `buf`;
    // `:mode`'s input is an ignored BLOB (see kernel provider/dev.rs).
    let r = unsafe { write_path(b"/dev/fb:mode\0", &mut buf, len, 0) };
    if r < 29 || buf[0] == 0 {
        return None;
    }
    Some(FbMode {
        present: buf[0] != 0,
        width: le_u32(&buf, 1),
        height: le_u32(&buf, 5),
        stride: le_u32(&buf, 9),
        bpp: le_u32(&buf, 13),
        pixel_format: le_u32(&buf, 17),
        size: le_u64(&buf, 21),
    })
}

/// Positionally write `data` into the framebuffer at byte `offset`, chunked
/// through a stack scratch so the caller's slice is never clobbered. Returns
/// total bytes written on success or -errno.
pub fn write_at(offset: u64, data: &[u8]) -> isize {
    let mut off = 0usize;
    while off < data.len() {
        let n = core::cmp::min(data.len() - off, 4096);
        let mut scratch = [0u8; 4096];
        scratch[..n].copy_from_slice(&data[off..off + n]);
        let r = unsafe { write_path(b"/dev/fb\0", &mut scratch, n, offset + off as u64) };
        if r < 0 {
            return r;
        }
        off += n;
    }
    data.len() as isize
}
