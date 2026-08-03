use common::types::PixelFormat;

use crate::color::Color;
use crate::display::Display;
use crate::font::FONT;

pub struct Framebuffer {
    fb_ptr: *mut u8,
    shadow: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
    bpp: u8,
    dirty: bool,
    dirty_x1: usize,
    dirty_y1: usize,
    dirty_x2: usize,
    dirty_y2: usize,
}

impl Framebuffer {
    pub unsafe fn new(
        addr: u64,
        width: usize,
        height: usize,
        stride: usize,
        pixel_format: PixelFormat,
        bpp: u8,
        shadow_addr: u64,
    ) -> Self {
        assert!(bpp > 0, "framebuffer bytes per pixel must be nonzero");
        assert!(width <= stride, "width must be <= stride (pixels per scanline)");

        // NOTE (Phase D): `addr` and `shadow_addr` are mapped VIRTUAL
        // addresses (never physical).  The framebuffer is reachable through
        // the VMM/UEFI mapping the kernel sets up, and the shadow lives on
        // the heap/guard-mapped arena — neither is an identity/physical deref.
        Framebuffer {
            fb_ptr: addr as *mut u8,
            shadow: shadow_addr as *mut u8,
            width,
            height,
            stride,
            pixel_format,
            bpp,
            dirty: false,
            dirty_x1: 0,
            dirty_y1: 0,
            dirty_x2: 0,
            dirty_y2: 0,
        }
    }

    /// Re-point the framebuffer base to `va` (e.g. a freshly mapped VMM
    /// window).  Used once the kernel page tables are live and the fb MMIO
    /// has a real mapping.
    pub fn set_fb_va(&mut self, va: u64) {
        self.fb_ptr = va as *mut u8;
    }

    /// Bind a new shadow buffer (a heap/VM-backed allocation) in place of the
    /// boot-time one.  The caller keeps ownership and lifetime.
    pub fn set_shadow_va(&mut self, va: u64) {
        self.shadow = va as *mut u8;
    }

    pub fn ptr(&self) -> *mut u8 {
        self.fb_ptr
    }

    pub fn shadow_ptr(&self) -> *mut u8 {
        self.shadow
    }

    pub fn as_bytes(&self) -> &[u8] {
        let len = self.total_bytes();
        unsafe { core::slice::from_raw_parts(self.fb_ptr, len) }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let len = self.total_bytes();
        unsafe { core::slice::from_raw_parts_mut(self.fb_ptr, len) }
    }

    pub fn shadow_as_slice(&self) -> &[u8] {
        let len = self.total_bytes();
        unsafe { core::slice::from_raw_parts(self.shadow, len) }
    }

    pub fn shadow_as_slice_mut(&mut self) -> &mut [u8] {
        let len = self.total_bytes();
        unsafe { core::slice::from_raw_parts_mut(self.shadow, len) }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    pub fn bpp(&self) -> u8 {
        self.bpp
    }

    pub fn total_bytes(&self) -> usize {
        self.stride
            .checked_mul(self.height)
            .and_then(|v| v.checked_mul(self.bpp as usize))
            .expect("framebuffer total_bytes overflow")
    }

    fn bpp_usize(&self) -> usize {
        self.bpp as usize
    }

    fn checked_offset(&self, x: usize, y: usize) -> Option<usize> {
        let bpp = self.bpp_usize();
        let row = y.checked_mul(self.stride)?.checked_mul(bpp)?;
        let col = x.checked_mul(bpp)?;
        let off = row.checked_add(col)?;
        if off < self.total_bytes() { Some(off) } else { None }
    }

    fn checked_row_offset(&self, y: usize) -> Option<usize> {
        let bpp = self.bpp_usize();
        let off = y.checked_mul(self.stride)?.checked_mul(bpp)?;
        if off < self.total_bytes() { Some(off) } else { None }
    }

    fn row_bytes(&self) -> usize {
        self.stride * self.bpp_usize()
    }

    fn mark_dirty(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let x2 = (x + w).min(self.width);
        let y2 = (y + h).min(self.height);
        if x2 <= x || y2 <= y {
            return;
        }
        if self.dirty {
            self.dirty_x1 = self.dirty_x1.min(x);
            self.dirty_y1 = self.dirty_y1.min(y);
            self.dirty_x2 = self.dirty_x2.max(x2);
            self.dirty_y2 = self.dirty_y2.max(y2);
        } else {
            self.dirty = true;
            self.dirty_x1 = x;
            self.dirty_y1 = y;
            self.dirty_x2 = x2;
            self.dirty_y2 = y2;
        }
    }

    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let x1 = self.dirty_x1;
        let y1 = self.dirty_y1;
        let x2 = self.dirty_x2;
        let y2 = self.dirty_y2;
        let bpp = self.bpp_usize();
        for y in y1..y2 {
            let Some(off) = self.checked_offset(x1, y) else { continue };
            let count = (x2 - x1) * bpp;
            if off + count > self.total_bytes() { continue; }
            unsafe {
                core::ptr::copy_nonoverlapping(self.shadow.add(off), self.fb_ptr.add(off), count);
            }
        }
        self.dirty = false;
    }

    pub fn flush_full(&mut self) {
        let total = self.total_bytes();
        unsafe {
            core::ptr::copy_nonoverlapping(self.shadow, self.fb_ptr, total);
        }
        self.dirty = false;
    }
}

impl Display for Framebuffer {
    fn draw_char(&mut self, x: usize, y: usize, ch: u8) -> bool {
        let ok = unsafe {
            draw_glyph_raw(
                self.shadow,
                self.stride,
                self.bpp,
                self.width,
                self.height,
                self.pixel_format,
                x,
                y,
                ch,
                Color::WHITE,
                Color::BLACK,
            )
        };
        if ok {
            self.mark_dirty(x, y, 8, 16);
        }
        ok
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: Color) -> bool {
        if self.shadow.is_null() || x >= self.width || y >= self.height {
            return false;
        }
        let Some(offset) = self.checked_offset(x, y) else { return false; };
        let pixel = color.to_pixel_bytes(self.pixel_format);
        let bpp = self.bpp_usize();
        unsafe {
            write_pixel_bytes(self.shadow, offset, bpp, &pixel);
        }
        self.mark_dirty(x, y, 1, 1);
        true
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        if self.shadow.is_null() || w == 0 || h == 0 {
            return;
        }
        let pixel = color.to_pixel_bytes(self.pixel_format);
        let bpp = self.bpp_usize();
        for row in 0..h {
            let py = y + row;
            if py >= self.height {
                break;
            }
            let Some(off) = self.checked_offset(x, py) else { continue };
            let mut col = 0;
            while col < w {
                let px = x + col;
                if px >= self.width {
                    break;
                }
                unsafe { write_pixel_bytes(self.shadow, off + col * bpp, bpp, &pixel); }
                col += 1;
            }
        }
        self.mark_dirty(x, y, w, h);
    }

    fn scroll_up(&mut self, rows: usize) {
        if self.shadow.is_null() || rows == 0 || rows >= self.height {
            if rows >= self.height && !self.shadow.is_null() {
                self.clear();
            }
            return;
        }
        let row_bytes = self.row_bytes();
        let Some(src_off) = self.checked_row_offset(rows) else { return };
        let copy_rows = self.height - rows;
        let Some(copy_bytes) = copy_rows.checked_mul(row_bytes) else { return };
        if src_off + copy_bytes > self.total_bytes() { return; }
        let zero_bytes = rows * row_bytes;
        if copy_bytes + zero_bytes > self.total_bytes() { return; }
        unsafe {
            core::ptr::copy(
                self.shadow.add(src_off),
                self.shadow,
                copy_bytes,
            );
            core::ptr::write_bytes(self.shadow.add(copy_bytes), 0, zero_bytes);
        }
        self.mark_dirty(0, 0, self.width, self.height);
        self.flush();
    }

    fn clear(&mut self) {
        if self.shadow.is_null() {
            return;
        }
        let total = self.total_bytes();
        unsafe {
            core::ptr::write_bytes(self.shadow, 0, total);
        }
        self.mark_dirty(0, 0, self.width, self.height);
    }

    fn flush(&mut self) {
        Framebuffer::flush(self);
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }
}

pub(crate) unsafe fn draw_glyph_raw(
    buf: *mut u8,
    stride: usize,
    bpp: u8,
    width: usize,
    height: usize,
    pixel_format: PixelFormat,
    x: usize,
    y: usize,
    ch: u8,
    fg: Color,
    bg: Color,
) -> bool {
    if buf.is_null() || x >= width || y >= height || ch >= 128 {
        return false;
    }

    let glyph = FONT[ch as usize];
    let bpp = bpp as usize;
    let fg_pixel = fg.to_pixel_bytes(pixel_format);
    let bg_pixel = bg.to_pixel_bytes(pixel_format);

    for row in 0..16 {
        let py = y + row;
        if py >= height {
            break;
        }
        let base = unsafe { buf.add(py * stride * bpp + x * bpp) };
        for col in 0..8 {
            let px = x + col;
            if px >= width {
                break;
            }
            let pixel = if glyph[row] & (1 << (7 - col)) != 0 {
                fg_pixel
            } else {
                bg_pixel
            };
            unsafe { write_pixel_bytes(base, col * bpp, bpp, &pixel); }
        }
    }
    true
}

/// Write a pixel of `bpp` bytes at `buf + offset`.  `px` is the 4-byte BGR/RGB
/// channel layout from [`Color::to_pixel_bytes`]; for `bpp < 4` only the first
/// `bpp` bytes are stored (the trailing alpha byte is dropped).
#[inline]
unsafe fn write_pixel_bytes(buf: *mut u8, offset: usize, bpp: usize, px: &[u8; 4]) {
    match bpp {
        4 => unsafe { *(buf.add(offset) as *mut u32) = u32::from_le_bytes(*px) },
        2 => unsafe { *(buf.add(offset) as *mut u16) = u16::from_le_bytes([px[0], px[1]]) },
        _ => {
            for i in 0..bpp {
                unsafe { *buf.add(offset + i) = px[i] };
            }
        }
    }
}
