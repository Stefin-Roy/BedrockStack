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

    pub fn ptr(&self) -> *mut u8 {
        self.fb_ptr
    }

    pub fn shadow_ptr(&self) -> *mut u8 {
        self.shadow
    }

    pub fn phys_addr(&self) -> u64 {
        self.fb_ptr as u64
    }

    pub fn shadow_phys_addr(&self) -> u64 {
        self.shadow as u64
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
        self.stride * self.height * (self.bpp as usize)
    }

    fn bpp_usize(&self) -> usize {
        self.bpp as usize
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
        let bpp = self.bpp_usize();
        let stride = self.stride;
        let x1 = self.dirty_x1;
        let y1 = self.dirty_y1;
        let x2 = self.dirty_x2;
        let y2 = self.dirty_y2;
        for y in y1..y2 {
            let off = y * stride * bpp + x1 * bpp;
            let count = (x2 - x1) * bpp;
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
        let bpp = self.bpp_usize();
        if self.shadow.is_null() || x >= self.width || y >= self.height {
            return false;
        }
        let pixel = color.to_pixel_u32(self.pixel_format);
        let offset = y * self.stride * bpp + x * bpp;
        unsafe {
            *(self.shadow.add(offset) as *mut u32) = pixel;
        }
        self.mark_dirty(x, y, 1, 1);
        true
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let bpp = self.bpp_usize();
        if self.shadow.is_null() || w == 0 || h == 0 {
            return;
        }
        let pixel = color.to_pixel_u32(self.pixel_format);
        for row in 0..h {
            let py = y + row;
            if py >= self.height {
                break;
            }
            let base = unsafe { self.shadow.add(py * self.stride * bpp + x * bpp) as *mut u32 };
            let mut col = 0;
            while col < w {
                let px = x + col;
                if px >= self.width {
                    break;
                }
                unsafe { *base.add(col) = pixel; }
                col += 1;
            }
        }
        self.mark_dirty(x, y, w, h);
    }

    fn scroll_up(&mut self, rows: usize) {
        let bpp = self.bpp_usize();
        if self.shadow.is_null() || rows == 0 || rows >= self.height {
            if rows >= self.height && !self.shadow.is_null() {
                self.clear();
            }
            return;
        }
        let row_bytes = self.stride * bpp;
        let copy_bytes = (self.height - rows) * row_bytes;
        unsafe {
            core::ptr::copy(
                self.shadow.add(rows * row_bytes),
                self.shadow,
                copy_bytes,
            );
            core::ptr::write_bytes(self.shadow.add(copy_bytes), 0, rows * row_bytes);
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
    let fg_pixel = fg.to_pixel_u32(pixel_format);
    let bg_pixel = bg.to_pixel_u32(pixel_format);

    for row in 0..16 {
        let py = y + row;
        if py >= height {
            break;
        }
        let base = unsafe { buf.add(py * stride * bpp + x * bpp) as *mut u32 };
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
            unsafe { *base.add(col) = pixel; }
        }
    }
    true
}
