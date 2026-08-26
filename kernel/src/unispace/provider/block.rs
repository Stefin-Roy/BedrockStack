//! Block device probing — enumeration under /dev (device finding).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    let block_dir = Arc::new(crate::unispace::dir::ServiceDir::new(Arc::new(BlockListObject)));
    crate::unispace::connect("/dev/block", block_dir.clone())?;
    crate::unispace::connect("/dev/block/count", Arc::new(BlockCountObject))?;
    // flat alias
    crate::unispace::connect("/dev/block_count", Arc::new(BlockCountObject))?;
    crate::unispace::connect("/sys/fs/mounts", Arc::new(MountsObject))?;
    Ok(())
}

static BLOCK_ENTRY: Schema = Schema::Struct(&[
    Field { name: "model", ty: &schema::SCHEMA_STR },
    Field { name: "sectors", ty: &schema::SCHEMA_U64 },
]);

static BLOCK_LIST: Schema = Schema::List(&BLOCK_ENTRY);

struct BlockListObject;
impl Object for BlockListObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &BLOCK_LIST }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        // Snapshot the device Arcs while holding the lock, then drop the lock
        // before allocating Strings / encoding — avoids holding the block-device
        // lock during heap allocation and reduces contention with hot-plug poll.
        let snapshot = {
            let devs = crate::filesystems::blockdriver::driver::BLOCK_DEVICES.lock();
            devs.clone()
        };
        let mut items = Vec::with_capacity(snapshot.len());
        for d in snapshot.iter() {
            items.push(Value::Struct(vec![
                Value::Str(alloc::string::String::from(d.model_string())),
                Value::U64(d.sector_count()),
            ]));
        }
        schema::encode_value(&Value::List(items), &BLOCK_LIST, out)
    }
}

struct BlockCountObject;
impl Object for BlockCountObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U32 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let n = crate::filesystems::blockdriver::driver::BLOCK_DEVICES.lock().len() as u64;
        schema::encode_value(&Value::U64(n), &schema::SCHEMA_U32, out)
    }
}

// ── /sys/fs/mounts ──
static MOUNT_ENTRY: Schema = Schema::Struct(&[
    Field { name: "drive", ty: &schema::SCHEMA_STR },
    Field { name: "fstype", ty: &schema::SCHEMA_STR },
]);
static MOUNT_LIST: Schema = Schema::List(&MOUNT_ENTRY);

struct MountsObject;
impl Object for MountsObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &MOUNT_LIST }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut items = Vec::new();
        for letter in ['A', 'B'] {
            if let Ok(m) = crate::filesystems::vfs::DRIVE_MAP.lookup(letter) {
                // fstype is not stored directly; infer from drive letter + existence
                let fstype = if letter == 'A' { "tmpfs" } else { "fat32" };
                let mut drive_str = alloc::string::String::new();
                drive_str.push(letter);
                items.push(Value::Struct(vec![
                    Value::Str(drive_str),
                    Value::Str(alloc::string::String::from(fstype)),
                    ]));
                let _ = m;
            }
        }
        schema::encode_value(&Value::List(items), &MOUNT_LIST, out)
    }
}
