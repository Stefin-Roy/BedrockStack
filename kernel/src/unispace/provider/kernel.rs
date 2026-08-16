//! `/kernel` provider — kernel services as unispace objects.
//!
//! The tree is a flat set of named kernel capabilities under `/kernel`, each a
//! leaf object whose methods are *actions* and whose value read is *state*:
//!
//! - `/kernel/timer` — the monotonic timer.  `read` yields ns since boot;
//!   `write(/kernel/timer:sleep, {ns})` parks the calling task for the given
//!   nanoseconds; `:sleep_ms` takes milliseconds; `:until` blocks until an
//!   absolute deadline.  The blocking methods park the current task through
//!   the cooperative scheduler (`crate::task`), which is x86_64-only, so this
//!   provider is registered on x86_64 builds only.
//!
//! Discipline unchanged: every read/write path returns `Result` — the sleep
//! parking is the only non-Rust control flow, and it never holds a unispace
//! lock across the switch (`resolve` hands back an owned `Arc`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::dir::SimpleDir;
use super::super::schema::{self, MethodDesc, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub static SLEEP_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "ns",
    ty: &schema::SCHEMA_U64,
}]);

pub static SLEEP_MS_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "ms",
    ty: &schema::SCHEMA_U64,
}]);

pub static UNTIL_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "deadline_ns",
    ty: &schema::SCHEMA_U64,
}]);

static TIMER_METHODS: [MethodDesc; 3] = [
    MethodDesc {
        name: "sleep",
        input: &SLEEP_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
    MethodDesc {
        name: "sleep_ms",
        input: &SLEEP_MS_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
    MethodDesc {
        name: "until",
        input: &UNTIL_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
];

/// Register the `/kernel` system (x86_64 only — the blocking methods walk the
/// scheduler's task registry, which does not exist on riscv64).
pub fn register() -> Result<(), UnispaceError> {
    let kernel = Arc::new(SimpleDir::new());
    kernel.insert("timer", Arc::new(TimerObject));
    super::super::register("kernel", kernel)
}

/// `/kernel/timer`: read = ns since boot (monotonic), methods park the caller.
struct TimerObject;

impl Object for TimerObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_U64
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &TIMER_METHODS
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::U64(crate::services::universal_timer::now_ns());
        schema::encode_value(&v, &schema::SCHEMA_U64, out)
    }

    fn invoke(&self, method: usize, v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let arg = arg_u64(&v, 0)?;
        let now = crate::services::universal_timer::now_ns();
        match method {
            0 => crate::task::sleep_until(now.saturating_add(arg)), // sleep(ns)
            1 => crate::task::sleep_until(now.saturating_add(arg.saturating_mul(1_000_000))), // sleep_ms(ms)
            2 => crate::task::sleep_until(arg), // until(deadline_ns)
            _ => return Err(UnispaceError::MethodNotFound),
        }
        Ok(())
    }
}

/// Extract a `u64` field from a struct-typed method input.
fn arg_u64(v: &Value, idx: usize) -> Result<u64, UnispaceError> {
    match v {
        Value::Struct(fields) => match fields.get(idx) {
            Some(Value::U64(n)) => Ok(*n),
            _ => Err(UnispaceError::SchemaMismatch),
        },
        _ => Err(UnispaceError::SchemaMismatch),
    }
}
