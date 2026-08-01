use crate::display::Display;

pub struct Console {
    display: *mut dyn Display,
    cursor_col: usize,
    cursor_row: usize,
    max_cols: usize,
    max_rows: usize,
}

impl Console {
    pub unsafe fn new(display: &mut dyn Display) -> Self {
        let w = display.width();
        let h = display.height();
        let max_cols = if w > 0 { w / 8 } else { 0 };
        let max_rows = if h > 0 { h / 16 } else { 0 };
        Console {
            display: unsafe { core::mem::transmute::<&mut dyn Display, *mut dyn Display>(display) },
            cursor_col: 0,
            cursor_row: 0,
            max_cols,
            max_rows,
        }
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

        self.display_mut().draw_char(x, y, ch);
    }

    fn scroll(&mut self) {
        if self.display_mut().height() <= 16 {
            return;
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

    /// Clear the display to the background colour and reset the cursor to the
    /// top-left corner.
    pub fn clear(&mut self) {
        self.display_mut().clear();
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.display_mut().flush();
    }

    /// Erase the character before the cursor and move the cursor back one
    /// column (Backspace key).
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            self.draw_char(b' ');
            self.display_mut().flush();
        }
    }

    /// Erase the character at the cursor position without moving (Delete key).
    pub fn delete(&mut self) {
        self.draw_char(b' ');
        self.display_mut().flush();
    }

    pub fn flush(&mut self) {
        self.display_mut().flush();
    }
}
