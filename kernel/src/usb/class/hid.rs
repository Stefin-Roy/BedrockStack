//! HID class driver — boot keyboard + boot mouse, with a report-descriptor
//! fallback for generic-protocol (non-boot) devices.
//!
//! Binds the interrupt-IN endpoint of a HID interface, registering it as an
//! xHCI transfer completion target at bind time and arming one read.  The
//! UInputL `poll` hook then *consumes* completed reports: when the target's
//! ready flag is armed it diffs the report against the previous state to emit
//! normalized `InputEvent`s and re-arms the next read.  Exactly one TRB is
//! ever in flight; an idle device simply NAKs and leaves the TRB pending, so
//! the poll hook stays non-blocking.
//!
//! A keyboard with an interrupt-OUT endpoint also drives its Num/Caps/Scroll
//! LEDs: the lock-key presses toggle a local LED bitmap which is written to a
//! DMA page and flushed through the OUT endpoint using the same "exactly one
//! TRB in flight" pattern, so the poll hook never blocks.
//!
//! Two paths select the report format:
//! - **Boot protocol** (subclass 1, protocol 1 = keyboard / 2 = mouse): the
//!   driver issues the class `SET_PROTOCOL` request on EP0, then decodes the
//!   fixed boot report (8-byte keyboard, 4-byte mouse) with no report
//!   descriptor.  The boot keyboard output report is the single LED byte.
//! - **Generic protocol** (subclass 0, or boot subclass with protocol 0): the
//!   driver fetches the HID descriptor (0x21) and report descriptor (0x22)
//!   over EP0, parses them (see [`crate::usb::class::hid_report`]) for the
//!   device kind, report length, and output-report length, and decodes the
//!   report boot-style.
//!
//! One keyboard and one mouse are supported in this phase; a second device of
//! the same kind is rejected by `init_interface`.

use crate::sync::PreemptMutex;

use crate::drivers::serial::SerialPort;
use crate::input;
use crate::input::event::{InputEvent, InputType};
use crate::input::keycode::KeyCode;
use crate::input::mouse::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, REL_WHEEL, REL_X, REL_Y};
use crate::services::dma::DmaAllocator;
use crate::usb::class::driver::{BoundUsbDevice, InterfaceResources, UsbClassDriver};
use crate::usb::class::hid_report;
use crate::usb::usb::{
    CLASS_HID, DESC_HID, DESC_REPORT, HID_PROTOCOL_MOUSE, HID_SUBCLASS_BOOT, SetupPacket,
};
use crate::usb::xhci::command;
use crate::usb::xhci::device;
use crate::usb::xhci::event;
use crate::usb::xhci::memory::{self, TrbRing};

/// Boot keyboard report: modifier byte, reserved byte, 6 key usages.
const HID_BOOT_KBD_REPORT_LEN: u32 = 8;
/// Boot mouse report: buttons, X delta, Y delta, wheel delta.
const HID_BOOT_MOUSE_REPORT_LEN: u32 = 4;
/// Boot keyboard output report: the single LED byte (HID usage page 0x08).
const HID_BOOT_KBD_OUTPUT_LEN: u32 = 1;
const HID_MAX_KEYS: usize = 6;

/// The device kind of a bound HID interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidKind {
    Keyboard,
    Mouse,
}

struct HidDeviceInner {
    doorbell_va: u64,
    slot_id: u8,
    dci: u8,
    ring: TrbRing,
    report_phys: u64,
    report_va: u64,
    device_id: u32,
    report_len: u32,
    /// LED output-endpoint state.  `out_dci == 0` means the device has no
    /// usable interrupt-OUT endpoint (mouse/tablet) and LEDs are skipped.
    out_dci: u8,
    out_ring: Option<TrbRing>,
    led_phys: u64,
    led_va: u64,
    led_len: u32,
    /// Current NumLock/CapsLock/ScrollLock bitmap (HID usage page 0x08).
    led_state: u8,
    /// A lock-key press changed `led_state` and it has not been flushed yet.
    led_dirty: bool,
    /// True while one LED OUT TRB is pending; rewritten only after its
    /// completion is consumed (exactly one TRB in flight, like the IN side).
    out_inflight: bool,
    /// Generic-protocol pointer (QEMU `usb-tablet`) reports absolute X/Y and
    /// a button byte; boot-protocol mice report buttons first.  Drives the
    /// mouse decode below.
    absolute_pointer: bool,
    prev_mods: u8,
    prev_keys: [u8; HID_MAX_KEYS],
    prev_buttons: u8,
    prev_x: u8,
    prev_y: u8,
}

static KEYBOARD: PreemptMutex<Option<HidDeviceInner>> = PreemptMutex::new(None);
static MOUSE: PreemptMutex<Option<HidDeviceInner>> = PreemptMutex::new(None);

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
        ep0_ring: &mut TrbRing,
    ) -> Result<BoundUsbDevice, &'static str> {
        let report_page = dma.alloc_page().ok_or("OOM for HID report page")?;

        // Decide the report format before moving `res.interrupt_in` (the
        // generic path borrows `res` for the EP0 descriptor fetches).
        let (kind, report_len, output_len, absolute_pointer) = if res.iface_subclass
            == HID_SUBCLASS_BOOT
            && res.iface_protocol != crate::usb::usb::HID_PROTOCOL_NONE
        {
            // Boot path: select the protocol on EP0, then decode the fixed
            // boot report.  `SET_PROTOCOL` is a no-data control transfer.
            // The boot keyboard output report is the single LED byte; boot
            // mice have no output report.
            let (kind, len, out_len) = match res.iface_protocol {
                crate::usb::usb::HID_PROTOCOL_KEYBOARD => (
                    HidKind::Keyboard,
                    HID_BOOT_KBD_REPORT_LEN,
                    HID_BOOT_KBD_OUTPUT_LEN,
                ),
                HID_PROTOCOL_MOUSE => (HidKind::Mouse, HID_BOOT_MOUSE_REPORT_LEN, 0),
                _ => return Err("unsupported HID boot protocol"),
            };
            let setup = SetupPacket::set_protocol(
                crate::usb::usb::HID_PROTOCOL_NONE as u16,
                res.iface_num as u16,
            );
            device::submit_control_transfer(
                ep0_ring,
                res.doorbell_va,
                res.slot_id,
                &setup,
                0,
                0,
                false,
            )?;
            (kind, len, out_len, false)
        } else {
            // Generic path: fetch and parse the report descriptor to learn
            // the kind and report lengths; the report is interpreted
            // boot-style, so a pointer decodes absolute coordinates.
            let scratch = dma.alloc_page().ok_or("OOM for HID descriptor page")?;
            let (kind, len, out_len) =
                fetch_report_info(ep0_ring, &res, scratch.phys, scratch.virt)?;
            (kind, len, out_len, kind == HidKind::Mouse)
        };

        let ep = res.interrupt_in.ok_or("HID needs interrupt IN endpoint")?;

        if kind == HidKind::Keyboard && KEYBOARD.lock().is_some() {
            return Err("one USB HID keyboard already bound");
        }
        if kind == HidKind::Mouse && MOUSE.lock().is_some() {
            return Err("one USB HID mouse already bound");
        }

        if !event::register_transfer_target(
            res.slot_id,
            ep.dci,
            report_page.virt,
            report_len,
            (1 << 1) | (1 << 13),
        ) {
            return Err("transfer target table full");
        }

        // Bind the interrupt-OUT endpoint for keyboard LED output.  Devices
        // without one (mouse/tablet) skip LEDs entirely.
        let mut out_dci = 0u8;
        let mut out_ring = None;
        let mut led_phys = 0u64;
        let mut led_va = 0u64;
        if output_len > 0 {
            if let Some(oep) = res.interrupt_out {
                let page = dma.alloc_page().ok_or("OOM for HID LED page")?;
                if event::register_transfer_target(
                    res.slot_id,
                    oep.dci,
                    page.virt,
                    output_len,
                    (1 << 1) | (1 << 13),
                ) {
                    led_phys = page.phys;
                    led_va = page.virt;
                    out_dci = oep.dci;
                    out_ring = Some(oep.ring);
                }
            }
        }

        let device_id = match kind {
            HidKind::Keyboard => {
                input::register_device("usb-hid-keyboard", input::CAP_KEYS, Some(hid_keyboard_poll))
            }
            HidKind::Mouse => {
                input::register_device("usb-hid-mouse", input::CAP_MOUSE, Some(hid_mouse_poll))
            }
        };

        let mut inner = HidDeviceInner {
            doorbell_va: res.doorbell_va,
            slot_id: res.slot_id,
            dci: ep.dci,
            ring: ep.ring,
            report_phys: report_page.phys,
            report_va: report_page.virt,
            device_id,
            report_len,
            out_dci,
            out_ring,
            led_phys,
            led_va,
            led_len: output_len,
            led_state: 0,
            led_dirty: false,
            out_inflight: false,
            absolute_pointer,
            prev_mods: 0,
            prev_keys: [0; HID_MAX_KEYS],
            prev_buttons: 0,
            prev_x: 0,
            prev_y: 0,
        };

        // Arm the first interrupt-IN read.  Exactly one TRB stays in flight;
        // the driver re-arms only after consuming a completion.
        arm_read(&mut inner);

        match kind {
            HidKind::Keyboard => *KEYBOARD.lock() = Some(inner),
            HidKind::Mouse => *MOUSE.lock() = Some(inner),
        }

        SerialPort::puts("[usb-hid] ");
        SerialPort::puts(match kind {
            HidKind::Keyboard => "keyboard",
            HidKind::Mouse => "mouse",
        });
        SerialPort::puts(" bound slot=");
        SerialPort::put_u64(res.slot_id as u64);
        SerialPort::puts(" dci=");
        SerialPort::put_u64(ep.dci as u64);
        SerialPort::puts(" report=");
        SerialPort::put_u64(report_len as u64);
        SerialPort::puts(" bytes");
        if out_dci != 0 {
            SerialPort::puts(" LED out_dci=");
            SerialPort::put_u64(out_dci as u64);
        }
        SerialPort::puts("\n");

        Ok(BoundUsbDevice::Input(device_id))
    }
}

/// Fetch the HID descriptor (0x21) for the report-descriptor length, then the
/// report descriptor (0x22), and parse both.  Returns the device kind, input
/// report byte length, and output report byte length (0 when the descriptor
/// has no Output items), or an error for unrecognized devices.
fn fetch_report_info(
    ep0_ring: &mut TrbRing,
    res: &InterfaceResources,
    buf_phys: u64,
    buf_va: u64,
) -> Result<(HidKind, u32, u32), &'static str> {
    let setup = SetupPacket::get_descriptor_interface(DESC_HID, 0, res.iface_num as u16, 9);
    device::submit_control_transfer(
        ep0_ring,
        res.doorbell_va,
        res.slot_id,
        &setup,
        buf_phys,
        9,
        true,
    )?;
    let hid = unsafe { core::slice::from_raw_parts(buf_va as *const u8, 9) };
    if hid[0] < 9 || hid[1] != DESC_HID {
        return Err("bad HID descriptor");
    }
    let report_len = u16::from_le_bytes([hid[7], hid[8]]);
    if report_len == 0 || report_len > 4096 {
        return Err("bad HID report descriptor length");
    }

    let setup =
        SetupPacket::get_descriptor_interface(DESC_REPORT, 0, res.iface_num as u16, report_len);
    device::submit_control_transfer(
        ep0_ring,
        res.doorbell_va,
        res.slot_id,
        &setup,
        buf_phys,
        report_len,
        true,
    )?;
    let rd = unsafe { core::slice::from_raw_parts(buf_va as *const u8, report_len as usize) };
    let info =
        hid_report::parse_report_descriptor(rd).ok_or("unrecognized HID report descriptor")?;
    Ok((info.kind, info.report_len as u32, info.output_len as u32))
}

fn arm_read(inner: &mut HidDeviceInner) {
    inner.ring.enqueue(&memory::make_normal_trb(
        inner.report_phys,
        inner.report_len,
    ));
    inner.ring.flush();
    command::ring_doorbell(inner.doorbell_va, inner.slot_id, inner.dci);
}

/// UInputL poll hook: non-blockingly consume one completed interrupt-IN read
/// on the keyboard and emit the diff against the previous report.  Also
/// drains/arms the LED OUT transfer.  Returns immediately when no completion
/// has armed the ready flag, and re-arms the next read only after a consumed
/// report is fully processed.
fn hid_keyboard_poll() {
    let mut kb = KEYBOARD.lock();
    let inner = match kb.as_mut() {
        Some(i) => i,
        None => return,
    };

    // LED flush is independent of IN data, so drain/arm OUT first — a pending
    // completion from the previous LED report must be consumed before the
    // buffer can be rewritten.
    flush_leds(inner);

    if event::take_interrupt_completion(inner.slot_id, inner.dci).is_none() {
        return;
    }

    let len = inner.report_len as usize;
    let report = unsafe { core::slice::from_raw_parts(inner.report_va as *const u8, len) };
    let mods = report[0];
    let keys: [u8; HID_MAX_KEYS] = if len >= 8 {
        match report[2..8].try_into() {
            Ok(k) => k,
            Err(_) => return,
        }
    } else {
        let mut k = [0u8; HID_MAX_KEYS];
        let n = (len.saturating_sub(2)).min(HID_MAX_KEYS);
        k[..n].copy_from_slice(&report[2..2 + n]);
        k
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
                track_lock_led(inner, kc, true);
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

    // Reflect lock-key LED changes made above (may be deferred one poll if
    // the previous OUT TRB is still in flight).
    flush_leds(inner);

    arm_read(inner);
}

/// Toggle the HID LED bitmap (usage page 0x08) when a lock key is pressed.
/// The OS keymap independently toggles the same locks from the identical
/// event stream, so the two stay in sync as long as every press is delivered.
fn track_lock_led(inner: &mut HidDeviceInner, kc: KeyCode, pressed: bool) {
    if !pressed {
        return;
    }
    let bit = match kc {
        KeyCode::NumLock => 0x01,
        KeyCode::CapsLock => 0x02,
        KeyCode::ScrollLock => 0x04,
        _ => return,
    };
    inner.led_state ^= bit;
    inner.led_dirty = true;
}

/// Write the LED output report and flush it through the interrupt-OUT
/// endpoint, non-blockingly.  Exactly one OUT TRB is in flight: the buffer is
/// only rewritten after the previous transfer's completion is consumed.  A
/// pending report that cannot be sent yet simply waits for the next poll.
fn flush_leds(inner: &mut HidDeviceInner) {
    if inner.out_dci == 0 {
        return;
    }
    if inner.out_inflight {
        if event::take_interrupt_completion(inner.slot_id, inner.out_dci).is_some() {
            inner.out_inflight = false;
        } else {
            return;
        }
    }
    if !inner.led_dirty {
        return;
    }
    let out = match inner.out_ring.as_mut() {
        Some(r) => r,
        None => return,
    };
    unsafe { core::ptr::write_volatile(inner.led_va as *mut u8, inner.led_state) };
    out.enqueue(&memory::make_normal_trb(inner.led_phys, inner.led_len));
    out.flush();
    command::ring_doorbell(inner.doorbell_va, inner.slot_id, inner.out_dci);
    inner.out_inflight = true;
    inner.led_dirty = false;
    if cfg!(feature = "usb_trace") {
        SerialPort::puts("[usb-hid] LED report 0x");
        SerialPort::put_hex(inner.led_state as u64);
        SerialPort::puts("\n");
    }
}

/// UInputL poll hook for the mouse: button diffs plus signed axis deltas.
/// Boot mice report `(buttons, dx, dy, wheel)`; generic absolute pointers
/// (QEMU `usb-tablet`) report `(x, y, wheel, buttons)`, which are diffed
/// against the previous report to recover deltas.
fn hid_mouse_poll() {
    let mut mouse = MOUSE.lock();
    let inner = match mouse.as_mut() {
        Some(i) => i,
        None => return,
    };

    if event::take_interrupt_completion(inner.slot_id, inner.dci).is_none() {
        return;
    }

    let report = unsafe {
        core::slice::from_raw_parts(inner.report_va as *const u8, inner.report_len as usize)
    };

    let (buttons, dx, dy, wheel) = if inner.absolute_pointer {
        let x = report.get(0).copied().unwrap_or(0);
        let y = report.get(1).copied().unwrap_or(0);
        let dx = x.wrapping_sub(inner.prev_x) as i8;
        let dy = y.wrapping_sub(inner.prev_y) as i8;
        inner.prev_x = x;
        inner.prev_y = y;
        (
            report.get(3).copied().unwrap_or(0) & 0x07,
            dx,
            dy,
            report.get(2).copied().unwrap_or(0) as i8,
        )
    } else {
        (
            report.get(0).copied().unwrap_or(0) & 0x07,
            report.get(1).copied().unwrap_or(0) as i8,
            report.get(2).copied().unwrap_or(0) as i8,
            report.get(3).copied().unwrap_or(0) as i8,
        )
    };

    for bit in 0..3u8 {
        let pressed = buttons & (1 << bit) != 0;
        let was_pressed = inner.prev_buttons & (1 << bit) != 0;
        if pressed != was_pressed {
            let code = match bit {
                0 => BTN_LEFT,
                1 => BTN_RIGHT,
                _ => BTN_MIDDLE,
            };
            submit_mouse(inner.device_id, code, pressed as i32);
        }
    }
    inner.prev_buttons = buttons;

    if dx != 0 {
        submit_mouse(inner.device_id, REL_X, dx as i32);
    }
    if dy != 0 {
        submit_mouse(inner.device_id, REL_Y, dy as i32);
    }
    if wheel != 0 {
        submit_mouse(inner.device_id, REL_WHEEL, wheel as i32);
    }

    arm_read(inner);
}

fn submit_key(device_id: u32, kc: KeyCode, value: i32) {
    input::submit_event(InputEvent::new(device_id, InputType::Key, kc.code(), value));
}

fn submit_mouse(device_id: u32, code: u32, value: i32) {
    input::submit_event(InputEvent::new(device_id, InputType::Mouse, code, value));
}

/// Boot-report modifier bit -> physical key code.  GUI bits (3, 7) are kept so
/// the OS can see the Super keys (the keymap already tracks `super_`).
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
