//! USB/xHCI introspection — lives under /drivers/usb and /dev/usb.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    crate::unispace::connect("/drivers/usb", Arc::new(UsbObject))?;
    crate::unispace::connect("/dev/usb/count", Arc::new(UsbCountObject))?;
    Ok(())
}

static USB_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "present", ty: &schema::SCHEMA_BOOL },
    Field { name: "irq_count", ty: &schema::SCHEMA_U64 },
]);

struct UsbObject;
impl Object for UsbObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &USB_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let (present, irq) = (crate::usb::xhci::controller_present(), crate::usb::xhci::event::irq_count());
        #[cfg(not(target_arch = "x86_64"))]
        let (present, irq) = (false, 0u64);
        let v = Value::Struct(vec![Value::Bool(present), Value::U64(irq)]);
        schema::encode_value(&v, &USB_SCHEMA, out)
    }
}

struct UsbCountObject;
impl Object for UsbCountObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U32 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let n = crate::usb::xhci::device_count() as u64;
        #[cfg(not(target_arch = "x86_64"))]
        let n = 0u64;
        schema::encode_value(&Value::U64(n), &schema::SCHEMA_U32, out)
    }
}
