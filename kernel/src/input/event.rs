//! Normalized input event model for UInputL.
//!
//! Hardware drivers never deliver input directly to a consumer — they submit
//! normalized [`InputEvent`]s to the UInputL core (`crate::input::submit_event`).
//! The core owns timestamps, device IDs, event routing and the event queue, so
//! no consumer cares whether a keystroke came from PS/2, USB HID or a future
//! neural keyboard.

/// The class of input an event belongs to.  Mirrors the `InputType` enum from
/// the UInputL design: one event type for every device class, so keyboards,
/// mice, touch panels and gamepads all flow through the same pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InputType {
    /// A key press/release/repeat.  `code` is a `KeyCode`, `value` is
    /// 1 = pressed, 0 = released, 2 = auto-repeat.
    Key = 1,
    /// Pointer movement or button.  `code` is an axis or button code.
    Mouse = 2,
    Touch = 3,
    Gamepad = 4,
    /// A free-floating axis (analog stick, wheel, slider).
    Axis = 5,
    Custom = 6,
}

/// A single normalized input event.
///
/// Field semantics follow the evdev model:
/// - `timestamp`: monotonic nanoseconds, stamped by `submit_event` (the core),
///   never by the driver.
/// - `device_id`: UInputL-owned id of the device that produced the event.
/// - `type_`: device class (`InputType`).
/// - `code`: normalized code within the class (`KeyCode` for keyboards, axis
///   codes for mice/gamepads).
/// - `value`: key 1/0/2, axis delta (signed), etc.
/// - `flags`: reserved for routing/focus metadata, zero today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub timestamp: u64,
    pub device_id: u32,
    pub type_: InputType,
    pub code: u32,
    pub value: i32,
    pub flags: u32,
}

impl InputEvent {
    /// A zeroed event, usable for static/const initialization of queues.
    pub const fn zero() -> Self {
        InputEvent {
            timestamp: 0,
            device_id: 0,
            type_: InputType::Custom,
            code: 0,
            value: 0,
            flags: 0,
        }
    }

    pub const fn new(device_id: u32, type_: InputType, code: u32, value: i32) -> Self {
        InputEvent {
            timestamp: 0,
            device_id,
            type_,
            code,
            value,
            flags: 0,
        }
    }

    pub fn with_timestamp(mut self, ts: u64) -> Self {
        self.timestamp = ts;
        self
    }
}
