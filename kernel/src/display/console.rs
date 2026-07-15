use crate::boot::PixelFormat;
use crate::display::framebuffer::FONT;

pub struct Console {
    fb_ptr: *mut u8,
    stride: usize,
    height: usize,
    pixel_format: PixelFormat,
    cursor_col: usize,
    cursor_row: usize,
    max_cols: usize,
    max_rows: usize,
}

impl Console {
    pub unsafe fn new(
        fb_ptr: *mut u8,
        width: usize,
        height: usize,
        stride: usize,
        pixel_format: PixelFormat,
    ) -> Self {
        Console {
            fb_ptr,
            stride,
            height,
            pixel_format,
            cursor_col: 0,
            cursor_row: 0,
            max_cols: if width > 0 { width / 8 } else { 0 },
            max_rows: if height > 0 { height / 16 } else { 0 },
        }
    }

    fn draw_char(&mut self, ch: u8) {
        let x = self.cursor_col * 8;
        let y = self.cursor_row * 16;

        if self.fb_ptr.is_null() || ch as usize >= FONT.len() || x >= self.stride || y >= self.height
        {
            return;
        }

        let glyph = FONT[ch as usize];

        for row in 0..16 {
            let py = y + row;
            if py >= self.height {
                break;
            }
            for col in 0..8 {
                let px = x + col;
                if px >= self.stride {
                    break;
                }
                let offset = py * self.stride * 4 + px * 4;
                unsafe {
                    if glyph[row] & (1 << (7 - col)) != 0 {
                        match self.pixel_format {
                            PixelFormat::Bgr => {
                                *self.fb_ptr.add(offset) = 0xFF;
                                *self.fb_ptr.add(offset + 1) = 0xFF;
                                *self.fb_ptr.add(offset + 2) = 0xFF;
                                *self.fb_ptr.add(offset + 3) = 0x00;
                            }
                            PixelFormat::Rgb => {
                                *self.fb_ptr.add(offset) = 0xFF;
                                *self.fb_ptr.add(offset + 1) = 0xFF;
                                *self.fb_ptr.add(offset + 2) = 0xFF;
                                *self.fb_ptr.add(offset + 3) = 0x00;
                            }
                        }
                    } else {
                        *self.fb_ptr.add(offset) = 0x00;
                        *self.fb_ptr.add(offset + 1) = 0x00;
                        *self.fb_ptr.add(offset + 2) = 0x00;
                        *self.fb_ptr.add(offset + 3) = 0x00;
                    }
                }
            }
        }
    }

    fn scroll(&mut self) {
        let row_bytes = self.stride * 4;
        let char_row_bytes = 16 * row_bytes;
        let total_height = self.height;
        if total_height <= 16 {
            return;
        }
        unsafe {
            core::ptr::copy(
                self.fb_ptr.add(char_row_bytes),
                self.fb_ptr,
                row_bytes * (total_height - 16),
            );
            core::ptr::write_bytes(
                self.fb_ptr.add(row_bytes * (total_height - 16)),
                0,
                char_row_bytes,
            );
        }
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.cursor_row += 1;
        if self.cursor_row >= self.max_rows {
            self.scroll();
            self.cursor_row = self.max_rows.saturating_sub(1);
        }
    }

    pub fn putc(&mut self, c: u8) {
        match c {
            b'\n' => self.newline(),
            b'\r' => self.cursor_col = 0,
            b'\t' => {
                let tab_stop = 8;
                self.cursor_col = (self.cursor_col + tab_stop) / tab_stop * tab_stop;
                if self.cursor_col >= self.max_cols {
                    self.newline();
                }
            }
            0x20..=0x7E => {
                self.draw_char(c);
                self.cursor_col += 1;
                if self.cursor_col >= self.max_cols {
                    self.newline();
                }
            }
            _ => {}
        }
    }

    pub fn puts(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.putc(b);
        }
    }
}
