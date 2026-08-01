//! Keymap — translates normalized key codes plus modifier state into
//! characters.
//!
//! This is the only place where layout logic lives.  Hardware drivers report
//! *physical* keys (`KeyCode::A`, `KeyCode::Digit1`, ...); the keymap decides
//! that Shift+A is `A`, Shift+CapsLock+B is `b`, Ctrl+C is `ETX`, and that the
//! keypad produces digits only while NumLock is active.  Swapping the layout
//! (Dvorak, a national layout) means replacing this module — nothing in a
//! driver changes.

use super::event::{InputEvent, InputType};
use super::keycode::KeyCode;

/// Modifier/lock state tracked by the keymap.
///
/// The keymap observes every key event so it can maintain this state itself;
/// drivers do not need to resolve anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keymap {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
    /// Windows/Super/Meta key.
    pub super_: bool,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::new()
    }
}

impl Keymap {
    pub const fn new() -> Self {
        Keymap {
            shift: false,
            ctrl: false,
            alt: false,
            caps: false,
            num: false,
            scroll: false,
            super_: false,
        }
    }

    /// Feed one event, returning the character it produces (`None` for
    /// modifiers, locks, navigation, function keys, releases, and empty
    /// lookups).
    ///
    /// Control keys map to their classic characters so consumers can echo or
    /// act on them uniformly: backspace `\x08`, tab `\t`, enter `\n`, escape
    /// `\x1b`, delete `\x7f`.
    pub fn feed(&mut self, ev: &InputEvent) -> Option<char> {
        if ev.type_ != InputType::Key {
            return None;
        }
        let Some(code) = KeyCode::from_code(ev.code) else {
            return None;
        };
        let pressed = ev.value != 0;

        match code {
            KeyCode::ShiftLeft | KeyCode::ShiftRight => {
                self.shift = pressed;
                return None;
            }
            KeyCode::ControlLeft | KeyCode::ControlRight => {
                self.ctrl = pressed;
                return None;
            }
            KeyCode::AltLeft | KeyCode::AltRight => {
                self.alt = pressed;
                return None;
            }
            KeyCode::SuperLeft | KeyCode::SuperRight => {
                self.super_ = pressed;
                return None;
            }
            KeyCode::CapsLock => {
                if pressed {
                    self.caps = !self.caps;
                }
                return None;
            }
            KeyCode::NumLock => {
                if pressed {
                    self.num = !self.num;
                }
                return None;
            }
            KeyCode::ScrollLock => {
                if pressed {
                    self.scroll = !self.scroll;
                }
                return None;
            }
            _ => {}
        }

        if !pressed {
            return None; // releases never produce a character
        }

        self.char_for(code)
    }

    fn char_for(&self, code: KeyCode) -> Option<char> {
        // Ctrl+letter → classic control character (0x01..=0x1a).
        if self.ctrl {
            if let Some(lower) = self.letter(code) {
                return Some((lower as u8 - b'a' + 1) as char);
            }
        }

        // Letters: Shift XOR CapsLock selects the case.
        if let Some(lower) = self.letter(code) {
            return if self.shift != self.caps {
                Some((lower as u8).to_ascii_uppercase() as char)
            } else {
                Some(lower)
            };
        }

        match code {
            KeyCode::Space => Some(' '),
            KeyCode::Enter | KeyCode::KeypadEnter => Some('\n'),
            KeyCode::Tab => Some('\t'),
            KeyCode::Backspace => Some('\x08'),
            KeyCode::Escape => Some('\x1b'),
            KeyCode::Delete => Some('\x7f'),
            KeyCode::Digit1 => Some(if self.shift { '!' } else { '1' }),
            KeyCode::Digit2 => Some(if self.shift { '@' } else { '2' }),
            KeyCode::Digit3 => Some(if self.shift { '#' } else { '3' }),
            KeyCode::Digit4 => Some(if self.shift { '$' } else { '4' }),
            KeyCode::Digit5 => Some(if self.shift { '%' } else { '5' }),
            KeyCode::Digit6 => Some(if self.shift { '^' } else { '6' }),
            KeyCode::Digit7 => Some(if self.shift { '&' } else { '7' }),
            KeyCode::Digit8 => Some(if self.shift { '*' } else { '8' }),
            KeyCode::Digit9 => Some(if self.shift { '(' } else { '9' }),
            KeyCode::Digit0 => Some(if self.shift { ')' } else { '0' }),
            KeyCode::Minus => Some(if self.shift { '_' } else { '-' }),
            KeyCode::Equal => Some(if self.shift { '+' } else { '=' }),
            KeyCode::LeftBrace => Some(if self.shift { '{' } else { '[' }),
            KeyCode::RightBrace => Some(if self.shift { '}' } else { ']' }),
            KeyCode::Backslash => Some(if self.shift { '|' } else { '\\' }),
            KeyCode::Semicolon => Some(if self.shift { ':' } else { ';' }),
            KeyCode::Apostrophe => Some(if self.shift { '"' } else { '\'' }),
            KeyCode::Grave => Some(if self.shift { '~' } else { '`' }),
            KeyCode::Comma => Some(if self.shift { '<' } else { ',' }),
            KeyCode::Dot => Some(if self.shift { '>' } else { '.' }),
            KeyCode::Slash => Some(if self.shift { '?' } else { '/' }),
            // Keypad: digits/operators only while NumLock is active (Shift
            // inverts the keypad decision, per the PS/2 spec).
            KeyCode::Keypad0 => self.keypad_digit('0'),
            KeyCode::Keypad1 => self.keypad_digit('1'),
            KeyCode::Keypad2 => self.keypad_digit('2'),
            KeyCode::Keypad3 => self.keypad_digit('3'),
            KeyCode::Keypad4 => self.keypad_digit('4'),
            KeyCode::Keypad5 => self.keypad_digit('5'),
            KeyCode::Keypad6 => self.keypad_digit('6'),
            KeyCode::Keypad7 => self.keypad_digit('7'),
            KeyCode::Keypad8 => self.keypad_digit('8'),
            KeyCode::Keypad9 => self.keypad_digit('9'),
            KeyCode::KeypadDecimal => self.keypad_digit('.'),
            KeyCode::KeypadDivide => self.keypad_digit('/'),
            KeyCode::KeypadMultiply => self.keypad_digit('*'),
            KeyCode::KeypadSubtract => self.keypad_digit('-'),
            KeyCode::KeypadAdd => self.keypad_digit('+'),
            // Everything else (F-keys, navigation, PrintScreen, Pause, Insert,
            // ...) produces no character.
            _ => None,
        }
    }

    /// A lowercase letter for the letter key codes.
    fn letter(&self, code: KeyCode) -> Option<char> {
        let b = match code {
            KeyCode::A => b'a',
            KeyCode::B => b'b',
            KeyCode::C => b'c',
            KeyCode::D => b'd',
            KeyCode::E => b'e',
            KeyCode::F => b'f',
            KeyCode::G => b'g',
            KeyCode::H => b'h',
            KeyCode::I => b'i',
            KeyCode::J => b'j',
            KeyCode::K => b'k',
            KeyCode::L => b'l',
            KeyCode::M => b'm',
            KeyCode::N => b'n',
            KeyCode::O => b'o',
            KeyCode::P => b'p',
            KeyCode::Q => b'q',
            KeyCode::R => b'r',
            KeyCode::S => b's',
            KeyCode::T => b't',
            KeyCode::U => b'u',
            KeyCode::V => b'v',
            KeyCode::W => b'w',
            KeyCode::X => b'x',
            KeyCode::Y => b'y',
            KeyCode::Z => b'z',
            _ => return None,
        };
        Some(b as char)
    }

    /// Keypad digit/operator while NumLock is active (Shift inverts it).
    fn keypad_digit(&self, ch: char) -> Option<char> {
        if self.num != self.shift {
            Some(ch)
        } else {
            None
        }
    }
}
