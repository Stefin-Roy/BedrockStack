//! PCI probing — enumeration lives under /dev (device finding).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    let pci_dir = Arc::new(crate::unispace::dir::ServiceDir::new(Arc::new(PciListObject)));
    crate::unispace::connect("/dev/pci", pci_dir.clone())?;
    crate::unispace::connect("/dev/pci/count", Arc::new(PciCountObject))?;
    // flat alias for tooling
    crate::unispace::connect("/dev/pci_count", Arc::new(PciCountObject))?;
    crate::unispace::connect("/sys/pci/ecam", Arc::new(EcamObject))?;
    Ok(())
}

// ── /dev/pci  list of devices ──
static PCI_ENTRY: Schema = Schema::Struct(&[
    Field { name: "segment", ty: &schema::SCHEMA_U32 },
    Field { name: "bus", ty: &schema::SCHEMA_U32 },
    Field { name: "device", ty: &schema::SCHEMA_U32 },
    Field { name: "function", ty: &schema::SCHEMA_U32 },
    Field { name: "vid", ty: &schema::SCHEMA_U32 },
    Field { name: "did", ty: &schema::SCHEMA_U32 },
    Field { name: "class", ty: &schema::SCHEMA_U32 },
    Field { name: "subclass", ty: &schema::SCHEMA_U32 },
    Field { name: "prog_if", ty: &schema::SCHEMA_U32 },
]);
static PCI_LIST: Schema = Schema::List(&PCI_ENTRY);

struct PciListObject;
impl Object for PciListObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PCI_LIST }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let devs = crate::pci::devices();
        let mut items = Vec::with_capacity(devs.len());
        for d in devs {
            items.push(Value::Struct(vec![
                Value::U64(d.segment as u64),
                Value::U64(d.bus as u64),
                Value::U64(d.device as u64),
                Value::U64(d.function as u64),
                Value::U64(d.vendor_id as u64),
                Value::U64(d.device_id as u64),
                Value::U64(d.class as u64),
                Value::U64(d.subclass as u64),
                Value::U64(d.prog_if as u64),
            ]));
        }
        schema::encode_value(&Value::List(items), &PCI_LIST, out)
    }
}

struct PciCountObject;
impl Object for PciCountObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U32 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        schema::encode_value(&Value::U64(crate::pci::devices().len() as u64), &schema::SCHEMA_U32, out)
    }
}

// ── /sys/pci/ecam RO copy (same as /sys/acpi/mcfg but via PCI view)
static ECAM_ENTRY: Schema = Schema::Struct(&[
    Field { name: "segment", ty: &schema::SCHEMA_U32 },
    Field { name: "bus_start", ty: &schema::SCHEMA_U32 },
    Field { name: "bus_end", ty: &schema::SCHEMA_U32 },
    Field { name: "base", ty: &schema::SCHEMA_U64 },
]);
static ECAM_LIST: Schema = Schema::List(&ECAM_ENTRY);
struct EcamObject;
impl Object for EcamObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &ECAM_LIST }
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
        schema::encode_value(&Value::List(items), &ECAM_LIST, out)
    }
}
