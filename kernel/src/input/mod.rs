//! UInputL — the unified input layer.
//!
//! Architecture (mirroring the design doc):
//!
//! ```text
//!                  +----------------+
//!                  |  User Process  |
//!                  +----------------+
//!                          |
//!                     UInputL API
//!                          |
//!             +------------+------------+
//!             |           |            |
//!        Event Queue  Subscribers  Grab/Focus
//!             |
//!             |
//!        +----+--------------------------------+
//!        |              UInputL Core           |
//!        +-------------------------------------+
//!                          |
//!                  Normalized Input Events
//!                          |
//!        +----------+-------------+-------------+
//!        |          |             |             |
//!     PS/2      USB HID      Bluetooth       (future)
//!   Keyboard     Keyboard     Gamepad
//! ```
//!
//! Drivers are *producers*: they register a device, then call
//! [`submit_event`] with normalized [`InputEvent`]s.  They never touch a
//! console, buffer a "keyboard" for a consumer, or care who consumes the
//! event.  Consumers ask the core for events via [`read_event`], or register a
//! [`subscribe`] callback.  [`grab_device`] lets a consumer take exclusive
//! ownership of a device's events (games, focus).

pub mod event;
pub mod keycode;
pub mod keymap;
pub mod mouse;
mod queue;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use spin::Mutex;

use keymap::Keymap;
use queue::InputQueue;

pub use event::{InputEvent, InputType};
pub use keycode::KeyCode;

// ── Device capabilities ───────────────────────────────────────────

pub const CAP_KEYS: u32 = 1 << 0;
pub const CAP_MOUSE: u32 = 1 << 1;
pub const CAP_TOUCH: u32 = 1 << 2;
pub const CAP_GAMEPAD: u32 = 1 << 3;
pub const CAP_AXIS: u32 = 1 << 4;

// ── Core state ────────────────────────────────────────────────────

/// The event queue is a plain `static const` (round-based Vyukov ring, all
/// slots start empty at seq 0) — no heap allocation, no runtime builder.
static QUEUE: InputQueue = InputQueue::new();
static OVERFLOWS: AtomicUsize = AtomicUsize::new(0);

/// A registered input device.  UInputL owns the `id`.
#[derive(Debug, Clone, Copy)]
pub struct InputDevice {
    pub id: u32,
    pub name: &'static str,
    pub capabilities: u32,
    /// Optional poll hook the core calls when the queue is empty and the
    /// device wants to generate events without an IRQ (e.g. PS/2 polled mode).
    pub poll: Option<fn()>,
}

static DEVICES: Mutex<Vec<InputDevice>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

struct Subscriber {
    type_: InputType,
    handler: fn(&InputEvent),
}

static SUBSCRIBERS: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());

/// Device currently grabbed by a consumer, or 0 for no grab.
static GRAB_DEVICE: AtomicU32 = AtomicU32::new(0);

// ── Public API ────────────────────────────────────────────────────

/// Initialise UInputL.  Must be called before any driver registers a device or
/// submits events.  The queue is static, so this is a no-op for state setup;
/// it exists for boot-sequence clarity and future state initialisation.
pub fn init() {
    crate::drivers::serial::SerialPort::puts("[uinput] core ready\n");
}

fn queue() -> &'static InputQueue {
    &QUEUE
}

/// Register an input device.  Returns the UInputL-owned device id.
pub fn register_device(name: &'static str, capabilities: u32, poll: Option<fn()>) -> u32 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    DEVICES.lock().push(InputDevice {
        id,
        name,
        capabilities,
        poll,
    });
    crate::drivers::serial::SerialPort::puts("[uinput] registered device ");
    crate::drivers::serial::SerialPort::put_u64(id as u64);
    crate::drivers::serial::SerialPort::puts(": ");
    crate::drivers::serial::SerialPort::puts(name);
    crate::drivers::serial::SerialPort::puts("\n");
    id
}

/// Remove a device.  Returns `false` if the id was not registered.
pub fn unregister_device(id: u32) -> bool {
    let mut devs = DEVICES.lock();
    if let Some(idx) = devs.iter().position(|d| d.id == id) {
        devs.remove(idx);
        true
    } else {
        false
    }
}

/// Number of registered devices.
pub fn device_count() -> usize {
    DEVICES.lock().len()
}

/// Snapshot of the registered device names (for diagnostics/tests).
pub fn device_names() -> Vec<&'static str> {
    DEVICES.lock().iter().map(|d| d.name).collect()
}

/// Snapshot of every registered device (`id`, `name`, `capabilities`).  The
/// unispace `/input` provider reads this to describe devices to ring 3.
pub fn device_snapshot() -> Vec<(u32, &'static str, u32)> {
    DEVICES
        .lock()
        .iter()
        .map(|d| (d.id, d.name, d.capabilities))
        .collect()
}

/// Submit an event from a driver.  The core stamps the timestamp (the driver
/// never needs a clock) and enqueues the event.  Returns `false` if the queue
/// was full (the event is dropped and the overflow counter bumped).  Safe to
/// call from interrupt context.
pub fn submit_event(ev: InputEvent) -> bool {
    let ev = ev.with_timestamp(crate::services::universal_timer::now_ns());
    if queue().push(ev) {
        true
    } else {
        OVERFLOWS.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// Read the next event for the focused consumer, or `None` if none is ready.
///
/// Events from grabbed-out devices are skipped.  Each delivered event is also
/// copied to every matching [`subscribe`] handler before being returned.
/// Non-blocking.  When the queue is empty, registered devices' `poll` hooks
/// are invoked so poll-driven drivers (e.g. PS/2 without a working IRQ) can
/// submit events.
pub fn read_event() -> Option<InputEvent> {
    let grab = GRAB_DEVICE.load(Ordering::Relaxed);
    loop {
        if let Some(ev) = queue().pop() {
            if grab != 0 && ev.device_id != grab {
                continue; // grabbed by another consumer — skip
            }
            // Notify subscribers (copies), then hand the event to the reader.
            let subs = SUBSCRIBERS.lock();
            for s in subs.iter() {
                if s.type_ == ev.type_ {
                    (s.handler)(&ev);
                }
            }
            return Some(ev);
        }
        // Queue empty — let poll-driven devices produce events.
        let mut fed = false;
        {
            let devs = DEVICES.lock();
            for d in devs.iter() {
                if let Some(poll) = d.poll {
                    (poll)();
                    fed = true;
                }
            }
        }
        if !fed {
            return None;
        }
        // The poll hooks may have submitted events; loop to pop one.  If the
        // hooks produced nothing, the next iteration returns None.
        if queue().len() == 0 {
            return None;
        }
    }
}

/// Subscribe a handler to a class of events.  Handlers are invoked from
/// `read_event` (consumer context), never from an ISR, so they may use normal
/// kernel services.
pub fn subscribe(type_: InputType, handler: fn(&InputEvent)) {
    SUBSCRIBERS.lock().push(Subscriber { type_, handler });
}

/// Take exclusive ownership of a device's events.  Set to 0 to release.
pub fn grab_device(id: u32) {
    GRAB_DEVICE.store(id, Ordering::Relaxed);
}

/// Release any active grab.
pub fn release_grab() {
    GRAB_DEVICE.store(0, Ordering::Relaxed);
}

/// Current grab owner (0 = none).
pub fn grab_owner() -> u32 {
    GRAB_DEVICE.load(Ordering::Relaxed)
}

/// Number of events dropped because the queue was full.
pub fn overflow_count() -> u64 {
    OVERFLOWS.load(Ordering::Relaxed) as u64
}

/// Number of events currently buffered.
pub fn queued_count() -> usize {
    queue().len()
}

/// A ready-made [`Keymap`] for consumers that want character translation.
pub fn keymap() -> Keymap {
    Keymap::new()
}
