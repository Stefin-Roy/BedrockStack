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
                    if ev.type_ == InputType::Mouse {
                        let mut line = [0u8; 40];
                        let mut n = 0;
                        for &c in b"mouse: btn=0x".iter() {
                            line[n] = c;
                            n += 1;
                        }
                        line[n] = hex_digit(((ev.code >> 8) & 0xF) as u8);
                        line[n + 1] = hex_digit((ev.code & 0xF) as u8);
                        line[n + 2] = b' ';
                        line[n + 3] = b'v';
                        line[n + 4] = b'=';
                        n += 5;
                        if ev.value < 0 {
                            line[n] = b'-';
                            n += 1;
                        }
                        n += write_dec(&mut line[n..], ev.value.unsigned_abs());
                        line[n] = b'\n';
                        con.puts(unsafe { core::str::from_utf8_unchecked(&line[..n + 1]) });
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

fn hex_digit(v: u8) -> u8 {
    if v < 10 {
        b'0' + v
    } else {
        b'a' + v - 10
    }
}

/// Write `v` in decimal into `buf`, returning the digit count.
fn write_dec(buf: &mut [u8], v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0;
    let mut v = v;
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    while v != 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    n
}
