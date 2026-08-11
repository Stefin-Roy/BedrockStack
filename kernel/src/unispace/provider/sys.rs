use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Schema, Value};
use super::super::dir::SimpleDir;
use super::super::{Object, ObjectKind, UnispaceError};

pub static PHYS_MEM: Schema = Schema::Struct(&[
    schema::Field {
        name: "total_frames",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "free_frames",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "phys_high",
        ty: &schema::SCHEMA_U64,
    },
]);

pub static FEATURES: Schema = Schema::List(&schema::SCHEMA_STR);

const VERSION: &str = "bedrockos 0.7 (unispace)";

/// Register the `/sys` system (kernel introspection objects).
pub fn register() -> Result<(), UnispaceError> {
    let sys = Arc::new(SimpleDir::new());
    sys.insert("version", Arc::new(VersionObject));
    sys.insert("phys_mem", Arc::new(PhysMemObject));
    sys.insert("cpus", Arc::new(CpusObject));
    sys.insert("features", Arc::new(FeaturesObject));
    super::super::register("sys", sys)
}

struct VersionObject;

impl Object for VersionObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_STR
    }

    fn methods(&self) -> &'static [schema::MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let v = Value::Str(String::from(VERSION));
        schema::encode_value(&v, &schema::SCHEMA_STR, out)
    }
}

struct PhysMemObject;

impl Object for PhysMemObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &PHYS_MEM
    }

    fn methods(&self) -> &'static [schema::MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let alloc = crate::mm::heap::get_phys_allocator_mut();
        let v = Value::Struct(vec![
            Value::U64(alloc.total_frames() as u64),
            Value::U64(alloc.free_frames() as u64),
            Value::U64(alloc.alloc_end()),
        ]);
        schema::encode_value(&v, &PHYS_MEM, out)
    }
}

struct CpusObject;

impl Object for CpusObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_U32
    }

    fn methods(&self) -> &'static [schema::MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let v = Value::U64(crate::smp::cpu_count() as u64);
        schema::encode_value(&v, &schema::SCHEMA_U32, out)
    }
}

struct FeaturesObject;

impl Object for FeaturesObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &FEATURES
    }

    fn methods(&self) -> &'static [schema::MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        let mut list = Vec::new();
        if cfg!(feature = "cpu_slow") {
            list.push(Value::Str(String::from("cpu_slow")));
        }
        if cfg!(feature = "forceslowlogging") {
            list.push(Value::Str(String::from("forceslowlogging")));
        }
        if cfg!(feature = "fat_trace") {
            list.push(Value::Str(String::from("fat_trace")));
        }
        if cfg!(feature = "usb_trace") {
            list.push(Value::Str(String::from("usb_trace")));
        }
        if cfg!(feature = "kernelmb2") {
            list.push(Value::Str(String::from("kernelmb2")));
        }
        let v = Value::List(list);
        schema::encode_value(&v, &FEATURES, out)
    }
}
