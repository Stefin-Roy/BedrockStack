//! Interactive keyboard test module.
//!
//! Runs last, after all other modules.  Prints a prompt, echoes every decoded
//! printable key to the framebuffer, and halts forever once the physical Esc
//! key is pressed.

use framebuffer::Console;
use framebuffer::Framebuffer;

use crate::drivers::ps2::{Key, KeyEvent};

use super::Module;

pub struct Ps2Test;

impl Module for Ps2Test {
    fn name(&self) -> &str {
        "ps2-test"
    }

    fn version(&self) -> &str {
        "0.2.0"
    }

    fn init(&self, display: &mut Framebuffer) -> Result<(), &'static str> {
        if !crate::drivers::ps2::is_present() {
            crate::drivers::serial::SerialPort::puts(
                "[ps2-test] no keyboard present — skipping\n",
            );
            return Ok(());
        }

        let mut con = unsafe { Console::new(display) };
        con.puts("Type anything (Esc to halt)...\n");

        loop {
            match crate::drivers::ps2::poll_key() {
                Some(KeyEvent::Press(Key::Escape)) => {
                    con.puts("\n[ps2-test] Esc pressed — halting.\n");
                    loop {
                        x86_64::instructions::hlt();
                    }
                }
                Some(KeyEvent::Press(key)) => {
                    if let Some(ch) = key.char_repr() {
                        con.putc_and_flush(ch as u8);
                    }
                }
                Some(KeyEvent::Release(_)) => {}
                None => {
                    // Sleep until the next keyboard interrupt wakes us.
                    x86_64::instructions::hlt();
                }
            }
        }
    }
}
