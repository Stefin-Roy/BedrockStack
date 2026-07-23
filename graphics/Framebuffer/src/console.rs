use crate::color::Color;
use crate::display::Display;

#[cfg(feature = "scrollback")]
use alloc::vec::Vec;
#[cfg(feature = "scrollback")]
use alloc::vec;

#[cfg(feature = "scrollback")]
const SCROLLBACK_LINES: usize = 1024;

pub struct Console {
    display: *mut dyn Display,
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
    pub unsafe fn new(display: &mut dyn Display) -> Self {
        let w = display.width();
        let h = display.height();
        let max_cols = if w > 0 { w / 8 } else { 0 };
        let max_rows = if h > 0 { h / 16 } else { 0 };
        #[cfg(feature = "scrollback")]
        let screen_chars = vec![b' '; max_cols * max_rows];
        Console {
            display: unsafe { core::mem::transmute::<&mut dyn Display, *mut dyn Display>(display) },
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

    fn display_mut(&mut self) -> &mut dyn Display {
        unsafe { &mut *self.display }
    }

    fn draw_char(&mut self, ch: u8) {
        let x = self.cursor_col * 8;
        let y = self.cursor_row * 16;

        if ch >= 128 || x >= self.display_mut().width() || y >= self.display_mut().height() {
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

        self.display_mut().draw_char(x, y, ch);
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
        if self.display_mut().height() <= 16 {
            return;
        }

        #[cfg(feature = "scrollback")]
        {
            if self.view_offset < 0 {
                self.push_scrollback_line();
                self.shift_screen_up();
            }
        }

        self.display_mut().scroll_up(16);
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

    pub fn putc_and_flush(&mut self, c: u8) {
        self.putc(c);
        self.display_mut().flush();
    }

    pub fn puts(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.putc(b);
        }
        self.display_mut().flush();
    }

    pub fn flush(&mut self) {
        self.display_mut().flush();
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
        let view_offset = self.view_offset;
        let max_rows = self.max_rows;
        let max_cols = self.max_cols;
        let scrollback_lines = self.scrollback_lines;
        let display_ptr = self.display;

        if view_offset < 0 {
            let display = unsafe { &mut *display_ptr };
            display.clear();
            for row in 0..max_rows {
                for col in 0..max_cols {
                    let idx = row * max_cols + col;
                    let ch = self.screen_chars[idx];
                    if ch != b' ' {
                        let x = col * 8;
                        let y = row * 16;
                        display.draw_char(x, y, ch);
                    }
                }
            }
            display.flush();
            return;
        }

        let start_line = view_offset as usize;
        let end_line = (start_line + max_rows).min(scrollback_lines);
        let display = unsafe { &mut *display_ptr };
        display.clear();
        for sb_line in start_line..end_line {
            let row = sb_line - start_line;
            let base = sb_line * max_cols;
            for col in 0..max_cols {
                let ch = self.scrollback[base + col];
                let x = col * 8;
                let y = row * 16;
                display.draw_char(x, y, ch);
            }
        }
        display.flush();
    }
}
