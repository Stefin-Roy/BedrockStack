//! Kerneldump introspection — lives under /kernel/dump.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::schema::{self, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    crate::unispace::connect("/kernel/dump/in_progress", Arc::new(InProgressObject))?;
    crate::unispace::connect("/kernel/dump/last", Arc::new(LastObject))?;
    Ok(())
}

static IN_PROGRESS_SCHEMA: Schema = Schema::List(&schema::SCHEMA_BOOL);
struct InProgressObject;
impl Object for InProgressObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &IN_PROGRESS_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::kerneldump::dump::dump_in_progress_snapshot();
        let items = snap.iter().map(|&b| Value::Bool(b)).collect();
        schema::encode_value(&Value::List(items), &IN_PROGRESS_SCHEMA, out)
    }
}

struct LastObject;
impl Object for LastObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_BLOB }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        // No persistent last dump ring; return empty blob.
        schema::encode_value(&Value::Bytes(alloc::vec::Vec::new()), &schema::SCHEMA_BLOB, out)
    }
}
