//! Interactive input test module.
//!
//! The first input-layer consumer: instead of asking "did PS/2 receive
//! anything?" it asks UInputL "give me input events."  Every decoded key event
//! flows through the keymap and is echoed to the framebuffer; Esc halts
//! forever.  Clears the screen first so the echo does not overwrite boot logs.

use framebuffer::Console;
use framebuffer::Framebuffer;

use crate::input::keymap::Keymap;
use crate::input::{InputType, KeyCode};

use super::Module;

pub struct InputTest;

impl Module for InputTest {
    fn name(&self) -> &str {
        "input-test"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn init(&self, display: &mut Framebuffer) -> Result<(), &'static str> {
        if crate::drivers::ps2::is_present() || crate::input::device_count() > 0 {
            // fall through — keyboard present
        } else {
            crate::drivers::serial::SerialPort::puts(
                "[input-test] no input device present — skipping\n",
            );
            return Ok(());
        }

        let mut con = unsafe { Console::new(display) };
        con.clear(); // all black — the echo owns the screen from here on
        con.puts("UInputL test — type anything (Esc to halt)\n");
        con.puts("Backspace erases; Delete erases under the cursor\n");

        let mut keymap = Keymap::new();

        loop {
            match crate::input::read_event() {
                Some(ev) => {
                    if ev.type_ != InputType::Key {
                        continue;
                    }
                    let Some(code) = KeyCode::from_code(ev.code) else {
                        continue;
                    };
                    if ev.value != 0 && code == KeyCode::Escape {
                        con.puts("\n[input-test] Esc pressed — halting.\n");
                        loop {
                            x86_64::instructions::hlt();
                        }
                    }
                    match keymap.feed(&ev) {
                        Some('\x08') => con.backspace(), // Backspace
                        Some('\x7f') => con.delete(),    // Delete
                        Some(ch) => con.putc_and_flush(ch as u8),
                        None => {}
                    }
                }
                None => {
                    // No event pending — wait for the next interrupt.
                    x86_64::instructions::hlt();
                }
            }
        }
    }
}
