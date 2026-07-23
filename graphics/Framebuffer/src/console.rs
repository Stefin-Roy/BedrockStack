use common::types::PixelFormat;

use crate::color::Color;
use crate::framebuffer::draw_glyph_raw;

#[cfg(feature = "scrollback")]
use alloc::vec::Vec;
#[cfg(feature = "scrollback")]
use alloc::vec;

#[cfg(feature = "scrollback")]
const SCROLLBACK_LINES: usize = 1024;

pub struct Console {
    fb_ptr: *mut u8,
    stride: usize,
    bpp: u8,
    height: usize,
    width: usize,
    pixel_format: PixelFormat,
    cursor_col: usize,
    cursor_row: usize,
    max_cols: usize,
    max_rows: usize,
    fg_color: Color,
    bg_color: Color,
    #[cfg(feature = "scrollback")]
    screen_chars: Vec<u8>,
    #[cfg(feature = "scrollback")]
    scrollback: Vec<u8>,
    #[cfg(feature = "scrollback")]
    scrollback_lines: usize,
    #[cfg(feature = "scrollback")]
    view_offset: isize,
}

impl Console {
    pub unsafe fn new(
        fb_ptr: *mut u8,
        width: usize,
        height: usize,
        stride: usize,
        pixel_format: PixelFormat,
        bpp: u8,
    ) -> Self {
        let max_cols = if width > 0 { width / 8 } else { 0 };
        let max_rows = if height > 0 { height / 16 } else { 0 };
        #[cfg(feature = "scrollback")]
        let screen_chars = vec![b' '; max_cols * max_rows];
        Console {
            fb_ptr,
            stride,
            bpp,
            height,
            width,
            pixel_format,
            cursor_col: 0,
            cursor_row: 0,
            max_cols,
            max_rows,
            fg_color: Color::WHITE,
            bg_color: Color::BLACK,
            #[cfg(feature = "scrollback")]
            screen_chars,
            #[cfg(feature = "scrollback")]
            scrollback: Vec::with_capacity(max_cols * SCROLLBACK_LINES),
            #[cfg(feature = "scrollback")]
            scrollback_lines: 0,
            #[cfg(feature = "scrollback")]
            view_offset: -1,
        }
    }

    pub fn set_colors(&mut self, fg: Color, bg: Color) {
        self.fg_color = fg;
        self.bg_color = bg;
    }

    fn draw_char(&mut self, ch: u8) {
        let x = self.cursor_col * 8;
        let y = self.cursor_row * 16;

        if self.fb_ptr.is_null() || ch >= 128 || x >= self.width || y >= self.height {
            return;
        }

        #[cfg(feature = "scrollback")]
        {
            if self.view_offset >= 0 {
                return;
            }
            let idx = self.cursor_row * self.max_cols + self.cursor_col;
            if idx < self.screen_chars.len() {
                self.screen_chars[idx] = ch;
            }
        }

        unsafe {
            draw_glyph_raw(
                self.fb_ptr,
                self.stride,
                self.bpp,
                self.width,
                self.height,
                self.pixel_format,
                x,
                y,
                ch,
                self.fg_color,
                self.bg_color,
            );
        }
    }

    #[cfg(feature = "scrollback")]
    fn push_scrollback_line(&mut self) {
        for col in 0..self.max_cols {
            let ch = self.screen_chars[col];
            self.scrollback.push(ch);
        }
        self.scrollback_lines += 1;
    }

    #[cfg(feature = "scrollback")]
    fn shift_screen_up(&mut self) {
        let line_len = self.max_cols;
        let total = line_len * self.max_rows;
        if total == 0 {
            return;
        }
        for row in 1..self.max_rows {
            let src = row * line_len;
            let dst = (row - 1) * line_len;
            for col in 0..line_len {
                self.screen_chars[dst + col] = self.screen_chars[src + col];
            }
        }
        let last_start = (self.max_rows - 1) * line_len;
        for col in 0..line_len {
            self.screen_chars[last_start + col] = b' ';
        }
    }

    fn scroll(&mut self) {
        let bpp = self.bpp as usize;
        let row_bytes = self.stride * bpp;
        let char_row_bytes = 16 * row_bytes;
        if self.height <= 16 {
            return;
        }

        #[cfg(feature = "scrollback")]
        {
            if self.view_offset < 0 {
                self.push_scrollback_line();
                self.shift_screen_up();
            }
        }

        let copy_bytes = row_bytes * (self.height - 16);
        unsafe {
            for i in 0..copy_bytes {
                self.fb_ptr.add(i).write_volatile(
                    self.fb_ptr.add(char_row_bytes + i).read_volatile(),
                );
            }
            for i in 0..char_row_bytes {
                self.fb_ptr.add(copy_bytes + i).write_volatile(0);
            }
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

    #[cfg(feature = "scrollback")]
    pub fn scroll_back(&mut self, lines: usize) {
        if self.scrollback_lines == 0 {
            return;
        }
        let target = self.view_offset - lines as isize;
        self.view_offset = target.max(-1).min(self.scrollback_lines as isize - 1);
        self.redraw_from_scrollback();
    }

    #[cfg(feature = "scrollback")]
    pub fn scroll_forward(&mut self, lines: usize) {
        let target = self.view_offset + lines as isize;
        if target >= -1 {
            self.view_offset = target.min(self.scrollback_lines as isize - 1);
            self.redraw_from_scrollback();
        }
    }

    #[cfg(feature = "scrollback")]
    pub fn reset_scroll(&mut self) {
        self.view_offset = -1;
        self.redraw_from_scrollback();
    }

    #[cfg(feature = "scrollback")]
    fn redraw_from_scrollback(&mut self) {
        let bpp = self.bpp as usize;
        let total = self.stride * self.height * bpp;
        unsafe {
            for i in 0..total {
                self.fb_ptr.add(i).write_volatile(0);
            }
        }

        if self.view_offset < 0 {
            for row in 0..self.max_rows {
                for col in 0..self.max_cols {
                    let idx = row * self.max_cols + col;
                    let ch = self.screen_chars[idx];
                    if ch != b' ' {
                        let x = col * 8;
                        let y = row * 16;
                        unsafe {
                            draw_glyph_raw(
                                self.fb_ptr,
                                self.stride,
                                self.bpp as u8,
                                self.width,
                                self.height,
                                self.pixel_format,
                                x,
                                y,
                                ch,
                                self.fg_color,
                                self.bg_color,
                            );
                        }
                    }
                }
            }
            return;
        }

        let start_line = self.view_offset as usize;
        let end_line = (start_line + self.max_rows).min(self.scrollback_lines);

        for sb_line in start_line..end_line {
            let row = sb_line - start_line;
            let base = sb_line * self.max_cols;
            for col in 0..self.max_cols {
                let ch = self.scrollback[base + col];
                let x = col * 8;
                let y = row * 16;
                unsafe {
                    draw_glyph_raw(
                        self.fb_ptr,
                        self.stride,
                        self.bpp as u8,
                        self.width,
                        self.height,
                        self.pixel_format,
                        x,
                        y,
                        ch,
                        self.fg_color,
                        self.bg_color,
                    );
                }
            }
        }
    }
}
