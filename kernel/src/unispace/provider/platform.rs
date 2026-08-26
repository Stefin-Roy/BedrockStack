//! Platform (APIC/IOAPIC/PIT) — split RO /sys vs RW /kernel.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    crate::unispace::connect("/sys/platform/apic", Arc::new(ApicRoObject))?;
    crate::unispace::connect("/kernel/platform/apic", Arc::new(ApicRwObject))?;
    crate::unispace::connect("/sys/platform/ioapic", Arc::new(IoapicRoObject))?;
    crate::unispace::connect("/sys/platform/pit", Arc::new(PitObject))?;
    crate::unispace::connect("/sys/platform/interrupts", Arc::new(InterruptsObject))?;
    Ok(())
}

static APIC_RO_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "tsc_hz", ty: &schema::SCHEMA_U64 },
    Field { name: "apic_hz", ty: &schema::SCHEMA_U64 },
    Field { name: "lapic_base", ty: &schema::SCHEMA_U64 },
    Field { name: "apic_id", ty: &schema::SCHEMA_U32 },
]);

struct ApicRoObject;
impl Object for ApicRoObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &APIC_RO_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let (tsc_hz, apic_hz, base, id) = (
            crate::platform::x86_64_pc::apic::tsc_hz(),
            crate::platform::x86_64_pc::apic::apic_hz(),
            crate::platform::x86_64_pc::apic::lapic_base(),
            crate::platform::x86_64_pc::apic::read_full_apic_id() as u64,
        );
        #[cfg(not(target_arch = "x86_64"))]
        let (tsc_hz, apic_hz, base, id) = (0u64, 0u64, 0u64, 0u64);
        let v = Value::Struct(vec![Value::U64(tsc_hz), Value::U64(apic_hz), Value::U64(base), Value::U64(id)]);
        schema::encode_value(&v, &APIC_RO_SCHEMA, out)
    }
}

static APIC_RW_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "tsc_now_ns", ty: &schema::SCHEMA_U64 },
    Field { name: "timer_init", ty: &schema::SCHEMA_U32 },
    Field { name: "timer_cur", ty: &schema::SCHEMA_U32 },
]);

struct ApicRwObject;
impl Object for ApicRwObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &APIC_RW_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let (now, init, cur) = (
            crate::platform::x86_64_pc::apic::tsc_now_ns(),
            crate::platform::x86_64_pc::apic::timer_init_count() as u64,
            crate::platform::x86_64_pc::apic::timer_current_count() as u64,
        );
        #[cfg(not(target_arch = "x86_64"))]
        let (now, init, cur) = (0u64, 0u64, 0u64);
        let v = Value::Struct(vec![Value::U64(now), Value::U64(init), Value::U64(cur)]);
        schema::encode_value(&v, &APIC_RW_SCHEMA, out)
    }
}

static IOAPIC_RO_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "present", ty: &schema::SCHEMA_BOOL },
]);

struct IoapicRoObject;
impl Object for IoapicRoObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &IOAPIC_RO_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let present = crate::acpi::global_snapshot()
            .map(|s| matches!(s.interrupt_model, crate::acpi::InterruptModel::Apic(ref apic) if !apic.io_apics.is_empty()))
            .unwrap_or(false);
        schema::encode_value(&Value::Struct(vec![Value::Bool(present)]), &IOAPIC_RO_SCHEMA, out)
    }
}

static PIT_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "hz", ty: &schema::SCHEMA_U64 },
]);

struct PitObject;
impl Object for PitObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PIT_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        // PIT hz is fixed
        schema::encode_value(&Value::Struct(vec![Value::U64(1193182)]), &PIT_SCHEMA, out)
    }
}

static INTR_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "vec34", ty: &schema::SCHEMA_U64 },
]);

struct InterruptsObject;
impl Object for InterruptsObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &INTR_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let v = crate::arch::x86_64::idt::vec34_count();
        #[cfg(not(target_arch = "x86_64"))]
        let v = 0u64;
        schema::encode_value(&Value::Struct(vec![Value::U64(v)]), &INTR_SCHEMA, out)
    }
}
