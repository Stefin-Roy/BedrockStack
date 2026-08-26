//! ACPI introspection — RO tables under /sys/acpi, controls under /kernel/acpi.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    crate::unispace::connect("/sys/acpi/cpus", Arc::new(CpusObject))?;
    crate::unispace::connect("/sys/acpi/mcfg", Arc::new(McfgObject))?;
    crate::unispace::connect("/sys/acpi/dmar", Arc::new(DmarObject))?;
    crate::unispace::connect("/sys/acpi/tables", Arc::new(TablesObject))?;
    crate::unispace::connect("/kernel/acpi/platform", Arc::new(PlatformObject))?;
    Ok(())
}

// ── /sys/acpi/cpus ──
static CPU_ENTRY: Schema = Schema::Struct(&[
    Field { name: "apic_id", ty: &schema::SCHEMA_U32 },
    Field { name: "enabled", ty: &schema::SCHEMA_BOOL },
]);
static CPU_LIST: Schema = Schema::List(&CPU_ENTRY);
struct CpusObject;
impl Object for CpusObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &CPU_LIST }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let cpus = crate::acpi::global_cpus();
        let items = cpus.into_iter().map(|(id, en)| Value::Struct(vec![Value::U64(id as u64), Value::Bool(en)])).collect();
        schema::encode_value(&Value::List(items), &CPU_LIST, out)
    }
}

// ── /sys/acpi/mcfg ──
static MCFG_ENTRY: Schema = Schema::Struct(&[
    Field { name: "segment", ty: &schema::SCHEMA_U32 },
    Field { name: "bus_start", ty: &schema::SCHEMA_U32 },
    Field { name: "bus_end", ty: &schema::SCHEMA_U32 },
    Field { name: "base", ty: &schema::SCHEMA_U64 },
]);
static MCFG_LIST: Schema = Schema::List(&MCFG_ENTRY);
struct McfgObject;
impl Object for McfgObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &MCFG_LIST }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::acpi::global_snapshot();
        let mut items = Vec::new();
        if let Some(acpi) = snap {
            for r in &acpi.pci_config_regions.regions {
                items.push(Value::Struct(vec![
                    Value::U64(r.pci_segment_group as u64),
                    Value::U64(r.bus_number_start as u64),
                    Value::U64(r.bus_number_end as u64),
                    Value::U64(r.base_address),
                ]));
            }
        }
        schema::encode_value(&Value::List(items), &MCFG_LIST, out)
    }
}

// ── /sys/acpi/dmar ──
static DMAR_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "present", ty: &schema::SCHEMA_BOOL },
    Field { name: "host_width", ty: &schema::SCHEMA_U32 },
    Field { name: "drhds", ty: &schema::SCHEMA_U32 },
    Field { name: "rmrrs", ty: &schema::SCHEMA_U32 },
]);
struct DmarObject;
impl Object for DmarObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &DMAR_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::acpi::global_snapshot();
        let (present, host_width, drhds, rmrrs) = if let Some(acpi) = snap {
            if let Some(dmar) = &acpi.dmar {
                (true, dmar.host_address_width as u64, dmar.drhds.len() as u64, dmar.rmrrs.len() as u64)
            } else {
                (false, 0, 0, 0)
            }
        } else {
            (false, 0, 0, 0)
        };
        let v = Value::Struct(vec![Value::Bool(present), Value::U64(host_width), Value::U64(drhds), Value::U64(rmrrs)]);
        schema::encode_value(&v, &DMAR_SCHEMA, out)
    }
}

// ── /sys/acpi/tables ── (SDT count)
struct TablesObject;
impl Object for TablesObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U32 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let cnt = crate::acpi::global_snapshot().map(|a| a.table_count as u64).unwrap_or(0);
        schema::encode_value(&Value::U64(cnt), &schema::SCHEMA_U32, out)
    }
}

// ── /kernel/acpi/platform ──
static PLATFORM_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "reset_supported", ty: &schema::SCHEMA_BOOL },
    Field { name: "reset_value", ty: &schema::SCHEMA_U32 },
    Field { name: "slp_typ_s5", ty: &schema::SCHEMA_U32 },
    Field { name: "has_reset_gas", ty: &schema::SCHEMA_BOOL },
]);
struct PlatformObject;
impl Object for PlatformObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PLATFORM_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::acpi::global_snapshot();
        let (reset_sup, reset_val, slp, has_gas) = if let Some(acpi) = snap {
            let p = &acpi.platform_info;
            (p.reset_supported, p.reset_value as u64, p.slp_typ_s5.unwrap_or(0xFF) as u64, p.reset_gas.is_some())
        } else {
            (false, 0, 0xFF, false)
        };
        let v = Value::Struct(vec![Value::Bool(reset_sup), Value::U64(reset_val), Value::U64(slp), Value::Bool(has_gas)]);
        schema::encode_value(&v, &PLATFORM_SCHEMA, out)
    }
}
