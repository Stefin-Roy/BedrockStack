pub mod console;
pub mod framebuffer;

/// Display trait for rendering text.
pub trait Display {
    /// Draw a single ASCII character at (x, y) pixel coordinates.
    ///
    /// Returns true if any pixels were drawn, false if out of bounds.
    fn draw_char(&mut self, x: usize, y: usize, ch: u8) -> bool;

    /// Clear the entire display to black.
    fn clear(&mut self);

    /// Get display width in pixels.
    fn width(&self) -> usize;

    /// Get display height in pixels.
    fn height(&self) -> usize;
}
