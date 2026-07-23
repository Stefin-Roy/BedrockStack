use common::types::PixelFormat;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color::new(0, 0, 0, 0);
    pub const BLACK: Color = Color::new(0, 0, 0, 255);
    pub const WHITE: Color = Color::new(255, 255, 255, 255);
    pub const RED: Color = Color::new(255, 0, 0, 255);
    pub const GREEN: Color = Color::new(0, 255, 0, 255);
    pub const BLUE: Color = Color::new(0, 0, 255, 255);
    pub const YELLOW: Color = Color::new(255, 255, 0, 255);
    pub const CYAN: Color = Color::new(0, 255, 255, 255);
    pub const MAGENTA: Color = Color::new(255, 0, 255, 255);
    pub const GRAY: Color = Color::new(128, 128, 128, 255);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    pub const fn from_rgb(rgb: u32) -> Self {
        Color {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
            a: 255,
        }
    }

    pub fn to_pixel_bytes(self, format: PixelFormat) -> [u8; 4] {
        match format {
            PixelFormat::Bgr => [self.b, self.g, self.r, self.a],
            PixelFormat::Rgb => [self.r, self.g, self.b, self.a],
        }
    }

    pub fn to_pixel_u32(self, format: PixelFormat) -> u32 {
        u32::from_le_bytes(self.to_pixel_bytes(format))
    }
}
