//! PS/2 driver introspection — lives under /drivers/ps2.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    crate::unispace::connect("/drivers/ps2", Arc::new(Ps2Object))?;
    crate::unispace::connect("/sys/input/ps2", Arc::new(Ps2RoObject))?;
    Ok(())
}

static PS2_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "present", ty: &schema::SCHEMA_BOOL },
    Field { name: "overflows", ty: &schema::SCHEMA_U64 },
]);

struct Ps2Object;
impl Object for Ps2Object {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PS2_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let (present, overflows) = (crate::drivers::ps2::is_present(), crate::drivers::ps2::overflow_count());
        #[cfg(not(target_arch = "x86_64"))]
        let (present, overflows) = (false, 0u64);
        let v = Value::Struct(vec![Value::Bool(present), Value::U64(overflows)]);
        schema::encode_value(&v, &PS2_SCHEMA, out)
    }
}

struct Ps2RoObject;
impl Object for Ps2RoObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PS2_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let (present, overflows) = (crate::drivers::ps2::is_present(), crate::drivers::ps2::overflow_count());
        #[cfg(not(target_arch = "x86_64"))]
        let (present, overflows) = (false, 0u64);
        let v = Value::Struct(vec![Value::Bool(present), Value::U64(overflows)]);
        schema::encode_value(&v, &PS2_SCHEMA, out)
    }
}
