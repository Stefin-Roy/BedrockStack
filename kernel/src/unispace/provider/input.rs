//! `/input` provider — the UInputL event surface exposed to ring 3.
//!
//! UInputL (`crate::input`) is a provider-agnostic core: PS/2 and USB HID
//! drivers `submit_event` normalized [`InputEvent`]s into a lock-free queue,
//! and never care who consumes them.  This provider is a thin, read-only
//! surface over that core:
//!
//! - `/input/devices`  — `List` of `{id, name, caps}` snapshots of the
//!   registered devices;
//! - `/input/events`   — `List` of drained events (`{timestamp, device,
//!   type, code, value}`); empty list when nothing is pending;
//! - `/input/overflows`— u64 count of events dropped on a full queue;
//! - `/input/kbd`      — a translated-text object: its value is the current
//!   keymap/modifier state, `:flush` discards pending events, and `:get`
//!   (x86_64 only) parks the calling task until a printable character is
//!   decoded.
//!
//! ## Discipline
//!
//! Every read path stays bounded (a fixed-drain cap on `/input/events`, a
//! device-count snapshot) and returns `Result` — event/device data can only
//! produce `Err`, never a panic (`panic = "abort"`).
//!
//! ## Arch gating
//!
//! The queue and events are arch-neutral and registered on every target.
//! Only the blocking `:get` needs the cooperative scheduler (`crate::task`),
//! which is x86_64-only; on other targets it returns `Unsupported`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use super::super::dir::SimpleDir;
use super::super::schema::{self, EnumVariant, Field, MethodDesc, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

#[cfg(target_arch = "x86_64")]
use crate::input::InputType;
use crate::input::keymap::Keymap;

// ── Schemas ──────────────────────────────────────────────────────────

/// `InputType` discriminants, in wire order (matches `crate::input`).
static INPUT_TYPE_VARIANTS: [EnumVariant; 6] = [
    EnumVariant {
        name: "key",
        value: 1,
    },
    EnumVariant {
        name: "mouse",
        value: 2,
    },
    EnumVariant {
        name: "touch",
        value: 3,
    },
    EnumVariant {
        name: "gamepad",
        value: 4,
    },
    EnumVariant {
        name: "axis",
        value: 5,
    },
    EnumVariant {
        name: "custom",
        value: 6,
    },
];

/// One drained [`InputEvent`]: `struct{ timestamp: u64, device: u32,
/// type: enum, code: u32, value: i32 }`.
static EVENT_ENTRY: Schema = Schema::Struct(&[
    Field {
        name: "timestamp",
        ty: &schema::SCHEMA_U64,
    },
    Field {
        name: "device",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "type",
        ty: &Schema::Enum(&INPUT_TYPE_VARIANTS),
    },
    Field {
        name: "code",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "value",
        ty: &schema::SCHEMA_I32,
    },
]);

/// `read(/input/events)`: a (possibly empty) list of events.
pub static EVENT_LIST: Schema = Schema::List(&EVENT_ENTRY);

/// One registered input device: `struct{ id: u32, name: str, caps: u32 }`.
static DEVICE_ENTRY: Schema = Schema::Struct(&[
    Field {
        name: "id",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "name",
        ty: &schema::SCHEMA_STR,
    },
    Field {
        name: "caps",
        ty: &schema::SCHEMA_U32,
    },
]);

/// `read(/input/devices)`: snapshot of every registered device.
pub static DEVICE_LIST: Schema = Schema::List(&DEVICE_ENTRY);

/// `read(/input/kbd)`: current keymap/modifier state.
pub static KBD_STATE: Schema = Schema::Struct(&[
    Field {
        name: "shift",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "ctrl",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "alt",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "caps",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "num",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "scroll",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "super_",
        ty: &schema::SCHEMA_BOOL,
    },
]);

/// `:get` output: the decoded character as a single byte.  `x00`-class
/// control characters (`\x08` backspace, `\x0a` enter, `\x1b` escape, `\x7f`
/// delete) are returned verbatim so a consumer can echo or act on them.
static SCHEMA_U8: Schema = Schema::U8;

static KBD_METHODS: [MethodDesc; 2] = [
    MethodDesc {
        name: "get",
        input: &schema::SCHEMA_UNIT,
        output: &SCHEMA_U8,
    },
    MethodDesc {
        name: "flush",
        input: &schema::SCHEMA_UNIT,
        output: &schema::SCHEMA_UNIT,
    },
];

// ── Registration ──────────────────────────────────────────────────────

/// Register the `/input` system.
pub fn register() -> Result<(), UnispaceError> {
    let input = Arc::new(SimpleDir::new());
    input.insert("devices", Arc::new(DevicesObject));
    input.insert("events", Arc::new(EventsObject));
    input.insert("overflows", Arc::new(OverflowsObject));
    input.insert("kbd", Arc::new(KbdObject::new()));
    super::super::register("input", input)
}

// ── `/input/devices` ──────────────────────────────────────────────────

/// Value read: the current device snapshot.
struct DevicesObject;

impl Object for DevicesObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &DEVICE_LIST
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let devices = crate::input::device_snapshot();
        let mut items = Vec::with_capacity(devices.len());
        for (id, name, caps) in devices {
            items.push(Value::Struct(vec![
                Value::U64(id as u64),
                Value::Str(String::from(name)),
                Value::U64(caps as u64),
            ]));
        }
        schema::encode_value(&Value::List(items), &DEVICE_LIST, out)
    }
}

// ── `/input/events` ───────────────────────────────────────────────────

/// Value read: drain up to `MAX_DRAIN` events (a hostile or fully-busy input
/// cannot inflate the response unboundedly).  Calls `read_event()`, which
/// runs registered device poll hooks when the queue is empty, so poll-driven
/// devices (PS/2 polled mode) keep producing events.
struct EventsObject;

const MAX_DRAIN: usize = 32;

impl Object for EventsObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &EVENT_LIST
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut items = Vec::with_capacity(MAX_DRAIN);
        for _ in 0..MAX_DRAIN {
            match crate::input::read_event() {
                Some(ev) => items.push(Value::Struct(vec![
                    Value::U64(ev.timestamp),
                    Value::U64(ev.device_id as u64),
                    Value::Enum(ev.type_ as u32),
                    Value::U64(ev.code as u64),
                    Value::I64(ev.value as i64),
                ])),
                None => break,
            }
        }
        schema::encode_value(&Value::List(items), &EVENT_LIST, out)
    }
}

// ── `/input/overflows` ────────────────────────────────────────────────

/// Value read: number of events dropped because the queue was full.
struct OverflowsObject;

impl Object for OverflowsObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_U64
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::U64(crate::input::overflow_count());
        schema::encode_value(&v, &schema::SCHEMA_U64, out)
    }
}

// ── `/input/kbd` ──────────────────────────────────────────────────────

/// Translated-text object: owns a [`Keymap`] and maps drained key events to
/// characters.  Modifier/lock state persists across calls, so the caller does
/// not implement layout logic.  Single-consumer: `:get`/`:flush` are the only
/// drains of `/input/events` when used interactively.
struct KbdObject {
    keymap: Mutex<Keymap>,
}

impl KbdObject {
    fn new() -> Self {
        KbdObject {
            keymap: Mutex::new(Keymap::new()),
        }
    }

    /// Drain available events through the keymap; return the first decoded
    /// character, or `None` when the queue runs empty.
    #[cfg(target_arch = "x86_64")]
    fn next_char(&self) -> Option<u8> {
        while let Some(ev) = crate::input::read_event() {
            if ev.type_ == InputType::Key {
                let mut km = self.keymap.lock();
                if let Some(ch) = km.feed(&ev) {
                    return Some(ch as u8);
                }
            }
        }
        None
    }
}

impl Object for KbdObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &KBD_STATE
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &KBD_METHODS
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let km = self.keymap.lock();
        let v = Value::Struct(vec![
            Value::Bool(km.shift),
            Value::Bool(km.ctrl),
            Value::Bool(km.alt),
            Value::Bool(km.caps),
            Value::Bool(km.num),
            Value::Bool(km.scroll),
            Value::Bool(km.super_),
        ]);
        schema::encode_value(&v, &KBD_STATE, out)
    }

    fn invoke(&self, method: usize, _v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                #[cfg(target_arch = "x86_64")]
                {
                    // Blocking get needs a running task to park; in pure
                    // kernel context (no current task) it would busy-spin, so
                    // refuse instead.
                    let pc = crate::smp::current_per_cpu();
                    if pc.current_task.is_null() {
                        return Err(UnispaceError::Unsupported);
                    }
                    // Blocking get: park until the keymap yields a character.
                    loop {
                        if let Some(ch) = self.next_char() {
                            let v = Value::U64(ch as u64);
                            schema::encode_value(&v, &SCHEMA_U8, out)?;
                            return Ok(());
                        }
                        // Queue empty — release the CPU a little, then re-drain.
                        crate::task::sleep_until(
                            crate::services::universal_timer::now_ns().saturating_add(2_000_000),
                        );
                    }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    let _ = out;
                    Err(UnispaceError::Unsupported)
                }
            }
            1 => {
                // Flush: discard every pending event, keeping keymap state.
                while crate::input::read_event().is_some() {}
                Ok(())
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}
