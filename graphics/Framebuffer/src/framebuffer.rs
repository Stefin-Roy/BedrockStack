use common::types::PixelFormat;

use crate::color::Color;
use crate::display::Display;
use crate::font::FONT;

pub struct Framebuffer {
    ptr: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
    bpp: u8,
}

impl Framebuffer {
    pub unsafe fn new(addr: u64, width: usize, height: usize, stride: usize, pixel_format: PixelFormat, bpp: u8) -> Self {
        if addr != 0 {
            assert!(addr as u64 % bpp as u64 == 0, "framebuffer address must be bpp-aligned");
        }
        assert!(width <= stride, "width must be <= stride (pixels per scanline)");

        Framebuffer {
            ptr: addr as *mut u8,
            width,
            height,
            stride,
            pixel_format,
            bpp,
        }
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn phys_addr(&self) -> u64 {
        self.ptr as u64
    }

    pub fn as_bytes(&self) -> &[u8] {
        let len = self.stride * self.height * (self.bpp as usize);
        unsafe { core::slice::from_raw_parts(self.ptr, len) }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let len = self.stride * self.height * (self.bpp as usize);
        unsafe { core::slice::from_raw_parts_mut(self.ptr, len) }
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
}

impl Display for Framebuffer {
    fn draw_char(&mut self, x: usize, y: usize, ch: u8) -> bool {
        unsafe {
            draw_glyph_raw(
                self.ptr,
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
        }
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: Color) -> bool {
        let bpp = self.bpp as usize;
        if self.ptr.is_null() || x >= self.width || y >= self.height {
            return false;
        }
        let offset = y * self.stride * bpp + x * bpp;
        let bytes = color.to_pixel_bytes(self.pixel_format);
        unsafe {
            self.ptr.add(offset).copy_from_nonoverlapping(bytes.as_ptr(), bpp);
        }
        true
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let bpp = self.bpp as usize;
        if self.ptr.is_null() || w == 0 || h == 0 {
            return;
        }
        for row in 0..h {
            let py = y + row;
            if py >= self.height {
                break;
            }
            for col in 0..w {
                let px = x + col;
                if px >= self.width {
                    break;
                }
                let offset = py * self.stride * bpp + px * bpp;
                let bytes = color.to_pixel_bytes(self.pixel_format);
                unsafe {
                    self.ptr.add(offset).copy_from_nonoverlapping(bytes.as_ptr(), bpp);
                }
            }
        }
    }

    fn scroll_up(&mut self, rows: usize) {
        let bpp = self.bpp as usize;
        if self.ptr.is_null() || rows == 0 || rows >= self.height {
            if rows >= self.height && !self.ptr.is_null() {
                self.clear();
            }
            return;
        }
        let row_bytes = self.stride * bpp;
        let src_offset = rows * row_bytes;
        let dst = self.ptr;
        let src = unsafe { self.ptr.add(src_offset) };
        let copy_bytes = (self.height - rows) * row_bytes;
        unsafe {
            core::ptr::copy(src, dst, copy_bytes);
            core::ptr::write_bytes(self.ptr.add(copy_bytes), 0, rows * row_bytes);
        }
    }

    fn clear(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        let total = self.stride * self.height * (self.bpp as usize);
        unsafe {
            core::ptr::write_bytes(self.ptr, 0, total);
        }
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }
}

pub(crate) unsafe fn draw_glyph_raw(
    fb_ptr: *mut u8,
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
    if fb_ptr.is_null() || x >= width || y >= height || ch >= 128 {
        return false;
    }

    let glyph = FONT[ch as usize];
    let bpp = bpp as usize;

    for row in 0..16 {
        let py = y + row;
        if py >= height {
            break;
        }
        for col in 0..8 {
            let px = x + col;
            if px >= width {
                break;
            }
            let offset = py * stride * bpp + px * bpp;
            let color = if glyph[row] & (1 << (7 - col)) != 0 {
                fg
            } else {
                bg
            };
            let bytes = color.to_pixel_bytes(pixel_format);
            unsafe {
                fb_ptr.add(offset).copy_from_nonoverlapping(bytes.as_ptr(), bpp);
            }
        }
    }
    true
}
