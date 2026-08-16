//! /dev provider — the device tree.
//!
//! `/dev/fb` is a write-through device: writes land directly in the scanout
//! framebuffer (no shadow-buffer flush), so a write is immediately visible.
//! The value is raw pixels in the native pixel format — rows of
//! `stride * bpp` bytes in `pixel_format`, never premultiplied or
//! color-converted.  The `flags` word is the byte offset: `read_value_flags`
//! reads a window of the framebuffer starting there, `write_value_flags`
//! writes bytes at that offset.  `:mode` returns the framebuffer geometry
//! (`{present, width, height, stride, bpp, pixel_format, size}`); `:clear`
//! zeroes the whole screen.  When no framebuffer is registered, value
//! reads/writes/clears return `NotFound` and `:mode` reports `present:false`
//! — never a panic.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::dir::SimpleDir;
use super::super::schema::{self, EnumVariant, Field, MethodDesc, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

static PIXEL_FORMAT_VARIANTS: [EnumVariant; 2] = [
    EnumVariant {
        name: "rgb",
        value: 1,
    },
    EnumVariant {
        name: "bgr",
        value: 2,
    },
];

/// `read(/dev/fb:mode)`: framebuffer geometry.
pub static FB_MODE: Schema = Schema::Struct(&[
    Field {
        name: "present",
        ty: &schema::SCHEMA_BOOL,
    },
    Field {
        name: "width",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "height",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "stride",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "bpp",
        ty: &schema::SCHEMA_U32,
    },
    Field {
        name: "pixel_format",
        ty: &Schema::Enum(&PIXEL_FORMAT_VARIANTS),
    },
    Field {
        name: "size",
        ty: &schema::SCHEMA_U64,
    },
]);

static FB_METHODS: [MethodDesc; 2] = [
    // `:mode` takes a BLOB input (ignored) so the caller passes its full
    // buffer size as the syscall length — the write syscall bounds the method
    // response copy by that length, so a UNIT input would deliver nothing.
    MethodDesc {
        name: "mode",
        input: &schema::SCHEMA_BLOB,
        output: &FB_MODE,
    },
    MethodDesc {
        name: "clear",
        input: &schema::SCHEMA_UNIT,
        output: &schema::SCHEMA_UNIT,
    },
];

const READ_CHUNK: usize = 65536;

/// Register the `/dev` system (device objects).  Currently only `/dev/fb`.
pub fn register() -> Result<(), UnispaceError> {
    let dev = Arc::new(SimpleDir::new());
    dev.insert("fb", Arc::new(FbObject));
    super::super::register("dev", dev)
}

/// `/dev/fb` — the boot framebuffer as a byte-addressable, write-through
/// device.  See the module docs for the flags / method semantics.
struct FbObject;

impl Object for FbObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_BLOB
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &FB_METHODS
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        self.read_value_flags(out, max, 0)
    }

    /// `flags` is the byte offset to start reading at.  An offset at or past
    /// the device end reads `0` bytes.
    fn read_value_flags(
        &self,
        out: &mut Vec<u8>,
        max: usize,
        flags: u64,
    ) -> Result<(), UnispaceError> {
        let dev = crate::display::get().ok_or(UnispaceError::NotFound)?;
        if flags >= dev.size {
            return Ok(());
        }
        let mut pos = flags;
        while pos < dev.size && out.len() < max {
            let mut chunk = vec![0u8; READ_CHUNK];
            let n = crate::display::read_at(pos, &mut chunk);
            if n == 0 {
                break;
            }
            let take = core::cmp::min(n, max - out.len());
            out.extend_from_slice(&chunk[..take]);
            pos += take as u64;
        }
        Ok(())
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        self.write_value_flags(v, 0)
    }

    /// `flags` is the byte offset to write at; a write at-or-past the device
    /// end (or overflowing it) is rejected.  Writes are write-through: they
    /// land directly in the scanout framebuffer.
    fn write_value_flags(&self, v: Value, flags: u64) -> Result<(), UnispaceError> {
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        let dev = crate::display::get().ok_or(UnispaceError::NotFound)?;
        let Some(end) = flags.checked_add(bytes.len() as u64) else {
            return Err(UnispaceError::InvalidArgument);
        };
        if end > dev.size {
            return Err(UnispaceError::InvalidArgument);
        }
        if crate::display::write_at(flags, &bytes) {
            Ok(())
        } else {
            // write_at cannot fail once bounds-checked; defend anyway.
            Err(UnispaceError::NotFound)
        }
    }

    fn invoke(&self, method: usize, _v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                let value = match crate::display::get() {
                    Some(dev) => Value::Struct(vec![
                        Value::Bool(true),
                        Value::U64(dev.width as u64),
                        Value::U64(dev.height as u64),
                        Value::U64(dev.stride as u64),
                        Value::U64(dev.bpp as u64),
                        Value::Enum(match dev.pixel_format {
                            common::types::PixelFormat::Rgb => 1,
                            common::types::PixelFormat::Bgr => 2,
                        }),
                        Value::U64(dev.size),
                    ]),
                    None => Value::Struct(vec![
                        Value::Bool(false),
                        Value::U64(0),
                        Value::U64(0),
                        Value::U64(0),
                        Value::U64(0),
                        // `present:false` is the real signal; emit a valid
                        // discriminant so the struct still encodes.
                        Value::Enum(1),
                        Value::U64(0),
                    ]),
                };
                schema::encode_value(&value, &FB_MODE, out)
            }
            1 => {
                if crate::display::clear() {
                    Ok(())
                } else {
                    Err(UnispaceError::NotFound)
                }
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}
