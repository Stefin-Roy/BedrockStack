//! HID class driver (boot-keyboard only).
//!
//! Binds the interrupt-IN endpoint of a HID interface, then polls it through
//! the UInputL `poll` hook: each poll submits one interrupt-IN read, waits for
//! the transfer, and diffs the 8-byte boot keyboard report against the
//! previous state to emit normalized `InputEvent`s.  The boot protocol
//! (subclass 1, protocol 1) needs no report descriptor and no SET_PROTOCOL,
//! so interface class alone is enough to probe.
//!
//! Only one keyboard is supported in this phase; a second HID keyboard is
//! rejected by `init_interface`.

use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::input;
use crate::input::event::{InputEvent, InputType};
use crate::input::keycode::KeyCode;
use crate::services::dma::DmaAllocator;
use crate::usb::class::driver::{BoundUsbDevice, InterfaceResources, UsbClassDriver};
use crate::usb::xhci::device;
use crate::usb::xhci::memory::TrbRing;
use crate::usb::usb::CLASS_HID;

/// Boot keyboard report: modifier byte, reserved byte, 6 key usages.
const HID_BOOT_REPORT_LEN: u32 = 8;
const HID_MAX_KEYS: usize = 6;

/// How long an idle poll waits for an interrupt-IN transfer.  Kept short so a
/// read on an empty queue returns quickly when no key is held.
const HID_POLL_TIMEOUT_NS: u64 = 250_000_000;

struct HidKeyboardInner {
    doorbell_va: u64,
    slot_id: u8,
    dci: u8,
    ring: TrbRing,
    report_phys: u64,
    report_va: u64,
    device_id: u32,
    prev_mods: u8,
    prev_keys: [u8; HID_MAX_KEYS],
}

static KEYBOARD: Mutex<Option<HidKeyboardInner>> = Mutex::new(None);

pub struct HidDriver;

impl UsbClassDriver for HidDriver {
    fn name(&self) -> &str {
        "usb-hid"
    }

    fn probe(&self, iface_class: u8, _subclass: u8, _protocol: u8) -> bool {
        iface_class == CLASS_HID
    }

    fn init_interface(
        &self,
        res: InterfaceResources,
        dma: &dyn DmaAllocator,
    ) -> Result<BoundUsbDevice, &'static str> {
        if KEYBOARD.lock().is_some() {
            return Err("one USB HID keyboard already bound");
        }

        let ep = res.interrupt_in.ok_or("HID keyboard needs interrupt IN endpoint")?;
        let page = dma.alloc_page().ok_or("OOM for HID report page")?;
        let device_id = input::register_device("usb-hid-keyboard", input::CAP_KEYS, Some(hid_keyboard_poll));

        *KEYBOARD.lock() = Some(HidKeyboardInner {
            doorbell_va: res.doorbell_va,
            slot_id: res.slot_id,
            dci: ep.dci,
            ring: ep.ring,
            report_phys: page.phys,
            report_va: page.virt,
            device_id,
            prev_mods: 0,
            prev_keys: [0; HID_MAX_KEYS],
        });

        SerialPort::puts("[usb-hid] keyboard bound slot=");
        SerialPort::put_u64(res.slot_id as u64);
        SerialPort::puts(" dci=");
        SerialPort::put_u64(ep.dci as u64);
        SerialPort::puts("\n");

        Ok(BoundUsbDevice::Input(device_id))
    }
}

/// UInputL poll hook: perform one interrupt-IN read and emit the diff against
/// the previous report.
fn hid_keyboard_poll() {
    let mut kb = KEYBOARD.lock();
    let inner = match kb.as_mut() {
        Some(i) => i,
        None => return,
    };

    if device::submit_interrupt(
        &mut inner.ring,
        inner.doorbell_va,
        inner.slot_id,
        inner.dci,
        inner.report_phys,
        HID_BOOT_REPORT_LEN,
        HID_POLL_TIMEOUT_NS,
    )
    .is_err()
    {
        return;
    }

    let report =
        unsafe { core::slice::from_raw_parts(inner.report_va as *const u8, HID_BOOT_REPORT_LEN as usize) };
    let mods = report[0];
    let keys: [u8; HID_MAX_KEYS] = match report[2..8].try_into() {
        Ok(k) => k,
        Err(_) => return,
    };

    for bit in 0..8u8 {
        let pressed = mods & (1 << bit) != 0;
        let was_pressed = inner.prev_mods & (1 << bit) != 0;
        if pressed != was_pressed {
            if let Some(kc) = modifier_keycode(bit) {
                submit_key(inner.device_id, kc, pressed as i32);
            }
        }
    }

    for &usage in keys.iter() {
        if usage != 0 && !inner.prev_keys.contains(&usage) {
            if let Some(kc) = usage_to_keycode(usage) {
                submit_key(inner.device_id, kc, 1);
            }
        }
    }

    for &usage in inner.prev_keys.iter() {
        if usage != 0 && !keys.contains(&usage) {
            if let Some(kc) = usage_to_keycode(usage) {
                submit_key(inner.device_id, kc, 0);
            }
        }
    }

    inner.prev_mods = mods;
    inner.prev_keys = keys;
}

fn submit_key(device_id: u32, kc: KeyCode, value: i32) {
    input::submit_event(InputEvent::new(device_id, InputType::Key, kc.code(), value));
}

/// Boot-report modifier bit -> physical key code.
fn modifier_keycode(bit: u8) -> Option<KeyCode> {
    match bit {
        0 => Some(KeyCode::ControlLeft),
        1 => Some(KeyCode::ShiftLeft),
        2 => Some(KeyCode::AltLeft),
        3 => Some(KeyCode::SuperLeft),
        4 => Some(KeyCode::ControlRight),
        5 => Some(KeyCode::ShiftRight),
        6 => Some(KeyCode::AltRight),
        7 => Some(KeyCode::SuperRight),
        _ => None,
    }
}

/// HID Keyboard/Keypad page (usage 0x07) -> physical key code.  Boot keyboards
/// report usages in boot order, so a simple table is all that is needed.
fn usage_to_keycode(usage: u8) -> Option<KeyCode> {
    match usage {
        0x04 => Some(KeyCode::A),
        0x05 => Some(KeyCode::B),
        0x06 => Some(KeyCode::C),
        0x07 => Some(KeyCode::D),
        0x08 => Some(KeyCode::E),
        0x09 => Some(KeyCode::F),
        0x0A => Some(KeyCode::G),
        0x0B => Some(KeyCode::H),
        0x0C => Some(KeyCode::I),
        0x0D => Some(KeyCode::J),
        0x0E => Some(KeyCode::K),
        0x0F => Some(KeyCode::L),
        0x10 => Some(KeyCode::M),
        0x11 => Some(KeyCode::N),
        0x12 => Some(KeyCode::O),
        0x13 => Some(KeyCode::P),
        0x14 => Some(KeyCode::Q),
        0x15 => Some(KeyCode::R),
        0x16 => Some(KeyCode::S),
        0x17 => Some(KeyCode::T),
        0x18 => Some(KeyCode::U),
        0x19 => Some(KeyCode::V),
        0x1A => Some(KeyCode::W),
        0x1B => Some(KeyCode::X),
        0x1C => Some(KeyCode::Y),
        0x1D => Some(KeyCode::Z),
        0x1E => Some(KeyCode::Digit1),
        0x1F => Some(KeyCode::Digit2),
        0x20 => Some(KeyCode::Digit3),
        0x21 => Some(KeyCode::Digit4),
        0x22 => Some(KeyCode::Digit5),
        0x23 => Some(KeyCode::Digit6),
        0x24 => Some(KeyCode::Digit7),
        0x25 => Some(KeyCode::Digit8),
        0x26 => Some(KeyCode::Digit9),
        0x27 => Some(KeyCode::Digit0),
        0x28 => Some(KeyCode::Enter),
        0x29 => Some(KeyCode::Escape),
        0x2A => Some(KeyCode::Backspace),
        0x2B => Some(KeyCode::Tab),
        0x2C => Some(KeyCode::Space),
        0x2D => Some(KeyCode::Minus),
        0x2E => Some(KeyCode::Equal),
        0x2F => Some(KeyCode::LeftBrace),
        0x30 => Some(KeyCode::RightBrace),
        0x31 => Some(KeyCode::Backslash),
        0x33 => Some(KeyCode::Semicolon),
        0x34 => Some(KeyCode::Apostrophe),
        0x35 => Some(KeyCode::Grave),
        0x36 => Some(KeyCode::Comma),
        0x37 => Some(KeyCode::Dot),
        0x38 => Some(KeyCode::Slash),
        0x39 => Some(KeyCode::CapsLock),
        0x3A => Some(KeyCode::F1),
        0x3B => Some(KeyCode::F2),
        0x3C => Some(KeyCode::F3),
        0x3D => Some(KeyCode::F4),
        0x3E => Some(KeyCode::F5),
        0x3F => Some(KeyCode::F6),
        0x40 => Some(KeyCode::F7),
        0x41 => Some(KeyCode::F8),
        0x42 => Some(KeyCode::F9),
        0x43 => Some(KeyCode::F10),
        0x44 => Some(KeyCode::F11),
        0x45 => Some(KeyCode::F12),
        0x46 => Some(KeyCode::PrintScreen),
        0x47 => Some(KeyCode::ScrollLock),
        0x48 => Some(KeyCode::Pause),
        0x49 => Some(KeyCode::Insert),
        0x4A => Some(KeyCode::Home),
        0x4B => Some(KeyCode::PageUp),
        0x4C => Some(KeyCode::Delete),
        0x4D => Some(KeyCode::End),
        0x4E => Some(KeyCode::PageDown),
        0x4F => Some(KeyCode::ArrowRight),
        0x50 => Some(KeyCode::ArrowLeft),
        0x51 => Some(KeyCode::ArrowDown),
        0x52 => Some(KeyCode::ArrowUp),
        0x53 => Some(KeyCode::NumLock),
        0x54 => Some(KeyCode::KeypadDivide),
        0x55 => Some(KeyCode::KeypadMultiply),
        0x56 => Some(KeyCode::KeypadSubtract),
        0x57 => Some(KeyCode::KeypadAdd),
        0x58 => Some(KeyCode::KeypadEnter),
        0x59 => Some(KeyCode::Keypad1),
        0x5A => Some(KeyCode::Keypad2),
        0x5B => Some(KeyCode::Keypad3),
        0x5C => Some(KeyCode::Keypad4),
        0x5D => Some(KeyCode::Keypad5),
        0x5E => Some(KeyCode::Keypad6),
        0x5F => Some(KeyCode::Keypad7),
        0x60 => Some(KeyCode::Keypad8),
        0x61 => Some(KeyCode::Keypad9),
        0x62 => Some(KeyCode::Keypad0),
        0x63 => Some(KeyCode::KeypadDecimal),
        0x65 => Some(KeyCode::Menu),
        // Modifiers sent as usages (non-boot fallback).
        0xE0 => Some(KeyCode::ControlLeft),
        0xE1 => Some(KeyCode::ShiftLeft),
        0xE2 => Some(KeyCode::AltLeft),
        0xE3 => Some(KeyCode::SuperLeft),
        0xE4 => Some(KeyCode::ControlRight),
        0xE5 => Some(KeyCode::ShiftRight),
        0xE6 => Some(KeyCode::AltRight),
        0xE7 => Some(KeyCode::SuperRight),
        _ => None,
    }
}
