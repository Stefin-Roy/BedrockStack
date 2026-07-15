use crate::color::Color;

pub trait Display {
    fn draw_char(&mut self, x: usize, y: usize, ch: u8) -> bool;
    fn put_pixel(&mut self, x: usize, y: usize, color: Color) -> bool;
    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color);
    fn scroll_up(&mut self, rows: usize);
    fn clear(&mut self);
    fn width(&self) -> usize;
    fn height(&self) -> usize;
}
