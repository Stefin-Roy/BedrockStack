#![no_std]
#[cfg(feature = "scrollback")]
extern crate alloc;

pub mod color;
pub mod console;
pub mod display;
pub(crate) mod font;
pub mod framebuffer;

pub use color::Color;
pub use console::Console;
pub use display::Display;
pub use framebuffer::Framebuffer;
