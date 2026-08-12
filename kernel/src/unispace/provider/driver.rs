use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::dir::SimpleDir;
use super::super::schema::{self, MethodDesc, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

use crate::drivers::serial::SerialPort;

/// Register the `/driver` system (kernel driver introspection objects).
pub fn register() -> Result<(), UnispaceError> {
    let driver = Arc::new(SimpleDir::new());
    driver.insert("debugserial", Arc::new(DebugSerialObject));
    super::super::register("driver", driver)
}

/// `/driver/debugserial`: read returns the full captured COM1 history,
/// write appends the payload to COM1 (and therefore to the capture log too).
struct DebugSerialObject;

impl Object for DebugSerialObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_BLOB
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        crate::drivers::serial::capture_bytes(out);
        out.truncate(core::cmp::min(max, out.len()));
        Ok(())
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        for &c in &bytes {
            SerialPort::putc(c);
        }
        Ok(())
    }
}
