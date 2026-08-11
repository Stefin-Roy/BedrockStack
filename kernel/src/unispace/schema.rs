use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::UnispaceError;

// Schema wire tags (self-describing binary).
pub const TAG_UNIT: u8 = 0x00;
pub const TAG_BOOL: u8 = 0x01;
pub const TAG_U8: u8 = 0x02;
pub const TAG_U16: u8 = 0x03;
pub const TAG_U32: u8 = 0x04;
pub const TAG_U64: u8 = 0x05;
pub const TAG_I8: u8 = 0x06;
pub const TAG_I16: u8 = 0x07;
pub const TAG_I32: u8 = 0x08;
pub const TAG_I64: u8 = 0x09;
pub const TAG_F32: u8 = 0x0A;
pub const TAG_F64: u8 = 0x0B;
pub const TAG_STR: u8 = 0x0C;
pub const TAG_BYTES: u8 = 0x0D;
pub const TAG_BLOB: u8 = 0x0E;
pub const TAG_STRUCT: u8 = 0x0F;
pub const TAG_LIST: u8 = 0x10;
pub const TAG_ENUM: u8 = 0x11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub ty: &'static Schema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumVariant {
    pub name: &'static str,
    pub value: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDesc {
    pub name: &'static str,
    pub input: &'static Schema,
    pub output: &'static Schema,
}

/// A statically-describable value shape.  All references are `'static`, so
/// schemas can be declared as `const`/`static` next to the objects they serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    Unit,
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Str,
    Bytes,
    /// Raw bytes consuming the entire remaining payload (no length prefix).
    Blob,
    Struct(&'static [Field]),
    List(&'static Schema),
    Enum(&'static [EnumVariant]),
}

impl Schema {
    fn tag(&self) -> u8 {
        match self {
            Schema::Unit => TAG_UNIT,
            Schema::Bool => TAG_BOOL,
            Schema::U8 => TAG_U8,
            Schema::U16 => TAG_U16,
            Schema::U32 => TAG_U32,
            Schema::U64 => TAG_U64,
            Schema::I8 => TAG_I8,
            Schema::I16 => TAG_I16,
            Schema::I32 => TAG_I32,
            Schema::I64 => TAG_I64,
            Schema::F32 => TAG_F32,
            Schema::F64 => TAG_F64,
            Schema::Str => TAG_STR,
            Schema::Bytes => TAG_BYTES,
            Schema::Blob => TAG_BLOB,
            Schema::Struct(_) => TAG_STRUCT,
            Schema::List(_) => TAG_LIST,
            Schema::Enum(_) => TAG_ENUM,
        }
    }
}

// Common leaf schemas.
pub const SCHEMA_UNIT: Schema = Schema::Unit;
pub const SCHEMA_BOOL: Schema = Schema::Bool;
pub const SCHEMA_U32: Schema = Schema::U32;
pub const SCHEMA_U64: Schema = Schema::U64;
pub const SCHEMA_STR: Schema = Schema::Str;
pub const SCHEMA_BYTES: Schema = Schema::Bytes;
pub const SCHEMA_BLOB: Schema = Schema::Blob;

// ── Runtime values ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    /// Field values in declaration order (field names live in the Schema).
    Struct(Vec<Value>),
    Enum(u32),
}

// ── Cursor (bounds-checked reader) ────────────────────────────────────

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn read(&mut self, n: usize) -> Result<&'a [u8], UnispaceError> {
        let end = self.pos.checked_add(n).ok_or(UnispaceError::DecodeError)?;
        if end > self.data.len() {
            return Err(UnispaceError::DecodeError);
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, UnispaceError> {
        Ok(self.read(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, UnispaceError> {
        let b = self.read(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u64(&mut self) -> Result<u64, UnispaceError> {
        let b = self.read(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn read_string(&mut self) -> Result<String, UnispaceError> {
        let len = self.read_u32()? as usize;
        let b = self.read(len)?;
        String::from_utf8(b.to_vec()).map_err(|_| UnispaceError::DecodeError)
    }
}

pub(crate) fn write_len_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

// ── Schema encoding (wire) ─────────────────────────────────────────────

pub fn encode_schema(s: &Schema, out: &mut Vec<u8>) {
    out.push(s.tag());
    match s {
        Schema::Struct(fields) => {
            out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
            for f in *fields {
                write_len_string(out, f.name);
                encode_schema(f.ty, out);
            }
        }
        Schema::List(elem) => encode_schema(elem, out),
        Schema::Enum(vars) => {
            out.extend_from_slice(&(vars.len() as u32).to_le_bytes());
            for v in *vars {
                out.extend_from_slice(&v.value.to_le_bytes());
                write_len_string(out, v.name);
            }
        }
        _ => {}
    }
}

// ── Schema decoding (owned form, for clients / self-test) ─────────────

#[derive(Debug, Clone)]
pub enum OwnedSchema {
    Unit,
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Str,
    Bytes,
    Blob,
    Struct(Vec<(String, OwnedSchema)>),
    List(Box<OwnedSchema>),
    Enum(Vec<(String, u32)>),
}

fn decode_schema_at(c: &mut Cursor) -> Result<OwnedSchema, UnispaceError> {
    let tag = c.read_u8()?;
    Ok(match tag {
        TAG_UNIT => OwnedSchema::Unit,
        TAG_BOOL => OwnedSchema::Bool,
        TAG_U8 => OwnedSchema::U8,
        TAG_U16 => OwnedSchema::U16,
        TAG_U32 => OwnedSchema::U32,
        TAG_U64 => OwnedSchema::U64,
        TAG_I8 => OwnedSchema::I8,
        TAG_I16 => OwnedSchema::I16,
        TAG_I32 => OwnedSchema::I32,
        TAG_I64 => OwnedSchema::I64,
        TAG_F32 => OwnedSchema::F32,
        TAG_F64 => OwnedSchema::F64,
        TAG_STR => OwnedSchema::Str,
        TAG_BYTES => OwnedSchema::Bytes,
        TAG_BLOB => OwnedSchema::Blob,
        TAG_STRUCT => {
            let n = c.read_u32()? as usize;
            let mut fields = Vec::new();
            for _ in 0..n {
                let name = c.read_string()?;
                let ty = decode_schema_at(c)?;
                fields.push((name, ty));
            }
            OwnedSchema::Struct(fields)
        }
        TAG_LIST => OwnedSchema::List(Box::new(decode_schema_at(c)?)),
        TAG_ENUM => {
            let n = c.read_u32()? as usize;
            let mut vars = Vec::new();
            for _ in 0..n {
                let val = c.read_u32()?;
                let name = c.read_string()?;
                vars.push((name, val));
            }
            OwnedSchema::Enum(vars)
        }
        _ => return Err(UnispaceError::DecodeError),
    })
}

pub fn decode_schema_bytes(data: &[u8]) -> Result<OwnedSchema, UnispaceError> {
    let mut c = Cursor { data, pos: 0 };
    let s = decode_schema_at(&mut c)?;
    if c.pos != data.len() {
        return Err(UnispaceError::DecodeError);
    }
    Ok(s)
}

/// Decode a `:method` payload (name + input schema + output schema).
pub fn decode_method_bytes(
    data: &[u8],
) -> Result<(String, OwnedSchema, OwnedSchema), UnispaceError> {
    let mut c = Cursor { data, pos: 0 };
    let name = c.read_string()?;
    let input = decode_schema_at(&mut c)?;
    let output = decode_schema_at(&mut c)?;
    if c.pos != data.len() {
        return Err(UnispaceError::DecodeError);
    }
    Ok((name, input, output))
}

/// Decode a `:desc` payload (kind tag + value schema + method table).
pub fn decode_object_bytes(
    data: &[u8],
) -> Result<(u8, OwnedSchema, Vec<(String, OwnedSchema, OwnedSchema)>), UnispaceError> {
    let mut c = Cursor { data, pos: 0 };
    let kind = c.read_u8()?;
    let value = decode_schema_at(&mut c)?;
    let n = c.read_u32()? as usize;
    let mut methods = Vec::new();
    for _ in 0..n {
        let name = c.read_string()?;
        let input = decode_schema_at(&mut c)?;
        let output = decode_schema_at(&mut c)?;
        methods.push((name, input, output));
    }
    if c.pos != data.len() {
        return Err(UnispaceError::DecodeError);
    }
    Ok((kind, value, methods))
}

// ── Value encoding / decoding (payload wire) ───────────────────────────

pub fn decode_value(data: &[u8], schema: &Schema) -> Result<Value, UnispaceError> {
    let mut c = Cursor { data, pos: 0 };
    let v = decode_value_at(&mut c, schema)?;
    if c.pos != data.len() {
        return Err(UnispaceError::DecodeError);
    }
    Ok(v)
}

fn decode_value_at(c: &mut Cursor, schema: &Schema) -> Result<Value, UnispaceError> {
    Ok(match schema {
        Schema::Unit => Value::Unit,
        Schema::Bool => Value::Bool(c.read_u8()? != 0),
        Schema::U8 => Value::U64(c.read_u8()? as u64),
        Schema::U16 => Value::U64(c.read_u32()? as u64 & 0xFFFF),
        Schema::U32 => Value::U64(c.read_u32()? as u64),
        Schema::U64 => Value::U64(c.read_u64()?),
        Schema::I8 => Value::I64(c.read_u8()? as i8 as i64),
        Schema::I16 => Value::I64(c.read_u32()? as i16 as i64),
        Schema::I32 => Value::I64(c.read_u32()? as i32 as i64),
        Schema::I64 => Value::I64(c.read_u64()? as i64),
        Schema::F32 => Value::F64(f32::from_bits(c.read_u32()?) as f64),
        Schema::F64 => Value::F64(f64::from_bits(c.read_u64()?)),
        Schema::Str => Value::Str(c.read_string()?),
        Schema::Bytes => {
            let len = c.read_u32()? as usize;
            let b = c.read(len)?;
            Value::Bytes(b.to_vec())
        }
        Schema::Blob => {
            let len = c.data.len() - c.pos;
            let b = c.read(len)?;
            Value::Bytes(b.to_vec())
        }
        Schema::Struct(fields) => {
            let mut vals = Vec::new();
            for f in *fields {
                vals.push(decode_value_at(c, f.ty)?);
            }
            Value::Struct(vals)
        }
        Schema::List(elem) => {
            let n = c.read_u32()? as usize;
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(decode_value_at(c, elem)?);
            }
            Value::List(items)
        }
        Schema::Enum(vars) => {
            let disc = c.read_u32()?;
            if !vars.iter().any(|v| v.value == disc) {
                return Err(UnispaceError::SchemaMismatch);
            }
            Value::Enum(disc)
        }
    })
}

pub fn encode_value(v: &Value, schema: &Schema, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    match schema {
        Schema::Unit => match v {
            Value::Unit => Ok(()),
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::Bool => match v {
            Value::Bool(b) => {
                out.push(*b as u8);
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::U8 => match v {
            Value::U64(n) if *n <= 0xFF => {
                out.push(*n as u8);
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::U16 => match v {
            Value::U64(n) if *n <= 0xFFFF => {
                out.extend_from_slice(&(*n as u16).to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::U32 => match v {
            Value::U64(n) if *n <= u32::MAX as u64 => {
                out.extend_from_slice(&(*n as u32).to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::U64 => match v {
            Value::U64(n) => {
                out.extend_from_slice(&n.to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::I8 => match v {
            Value::I64(n) if *n >= i8::MIN as i64 && *n <= i8::MAX as i64 => {
                out.push(*n as u8);
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::I16 => match v {
            Value::I64(n) if *n >= i16::MIN as i64 && *n <= i16::MAX as i64 => {
                out.extend_from_slice(&(*n as i16).to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::I32 => match v {
            Value::I64(n) if *n >= i32::MIN as i64 && *n <= i32::MAX as i64 => {
                out.extend_from_slice(&(*n as i32).to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::I64 => match v {
            Value::I64(n) => {
                out.extend_from_slice(&n.to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::F32 => match v {
            Value::F64(f) => {
                out.extend_from_slice(&(*f as f32).to_bits().to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::F64 => match v {
            Value::F64(f) => {
                out.extend_from_slice(&f.to_bits().to_le_bytes());
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::Str => match v {
            Value::Str(s) => {
                write_len_string(out, s);
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::Bytes => match v {
            Value::Bytes(b) => {
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::Blob => match v {
            Value::Bytes(b) => {
                out.extend_from_slice(b);
                Ok(())
            }
            _ => Err(UnispaceError::SchemaMismatch),
        },
        Schema::Struct(fields) => {
            let vals = match v {
                Value::Struct(vals) => vals,
                _ => return Err(UnispaceError::SchemaMismatch),
            };
            if vals.len() != fields.len() {
                return Err(UnispaceError::SchemaMismatch);
            }
            for (f, val) in fields.iter().zip(vals) {
                encode_value(val, f.ty, out)?;
            }
            Ok(())
        }
        Schema::List(elem) => {
            let items = match v {
                Value::List(items) => items,
                _ => return Err(UnispaceError::SchemaMismatch),
            };
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for it in items {
                encode_value(it, elem, out)?;
            }
            Ok(())
        }
        Schema::Enum(vars) => {
            let disc = match v {
                Value::Enum(d) => *d,
                _ => return Err(UnispaceError::SchemaMismatch),
            };
            if !vars.iter().any(|var| var.value == disc) {
                return Err(UnispaceError::SchemaMismatch);
            }
            out.extend_from_slice(&disc.to_le_bytes());
            Ok(())
        }
    }
}

// ── Text rendering (debug) ─────────────────────────────────────────────

fn u64_str(n: u64) -> String {
    if n == 0 {
        return String::from("0");
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    let mut v = n;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    String::from_utf8(buf[i..].to_vec()).unwrap_or_default()
}

fn i64_str(n: i64) -> String {
    if n < 0 {
        let mut s = String::from("-");
        s.push_str(&u64_str((n as i128).unsigned_abs() as u64));
        s
    } else {
        u64_str(n as u64)
    }
}

pub fn text_of_owned(s: &OwnedSchema) -> String {
    match s {
        OwnedSchema::Unit => String::from("unit"),
        OwnedSchema::Bool => String::from("bool"),
        OwnedSchema::U8 => String::from("u8"),
        OwnedSchema::U16 => String::from("u16"),
        OwnedSchema::U32 => String::from("u32"),
        OwnedSchema::U64 => String::from("u64"),
        OwnedSchema::I8 => String::from("i8"),
        OwnedSchema::I16 => String::from("i16"),
        OwnedSchema::I32 => String::from("i32"),
        OwnedSchema::I64 => String::from("i64"),
        OwnedSchema::F32 => String::from("f32"),
        OwnedSchema::F64 => String::from("f64"),
        OwnedSchema::Str => String::from("str"),
        OwnedSchema::Bytes => String::from("bytes"),
        OwnedSchema::Blob => String::from("blob"),
        OwnedSchema::Struct(fields) => {
            let mut t = String::from("struct{");
            for (i, (name, ty)) in fields.iter().enumerate() {
                if i > 0 {
                    t.push_str(", ");
                }
                t.push_str(name);
                t.push_str(": ");
                t.push_str(&text_of_owned(ty));
            }
            t.push('}');
            t
        }
        OwnedSchema::List(elem) => {
            let mut t = String::from("list<");
            t.push_str(&text_of_owned(elem));
            t.push('>');
            t
        }
        OwnedSchema::Enum(vars) => {
            let mut t = String::from("enum{");
            for (i, (name, val)) in vars.iter().enumerate() {
                if i > 0 {
                    t.push_str(", ");
                }
                t.push_str(name);
                t.push_str("=");
                t.push_str(&u64_str(*val as u64));
            }
            t.push('}');
            t
        }
    }
}

/// Textual rendering of a value, using `schema` for field/variant names.
pub fn value_text(v: &Value, schema: &Schema) -> String {
    match (v, schema) {
        (Value::Struct(vals), Schema::Struct(fields)) => {
            let mut t = String::from("{");
            for (i, (f, val)) in fields.iter().zip(vals).enumerate() {
                if i > 0 {
                    t.push_str(", ");
                }
                t.push_str(f.name);
                t.push_str(": ");
                t.push_str(&value_text(val, f.ty));
            }
            t.push('}');
            t
        }
        (Value::List(items), Schema::List(elem)) => {
            let mut t = String::from("[");
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    t.push_str(", ");
                }
                t.push_str(&value_text(it, elem));
            }
            t.push(']');
            t
        }
        (Value::Enum(d), Schema::Enum(vars)) => {
            for v in *vars {
                if v.value == *d {
                    return String::from(v.name);
                }
            }
            alloc::format!("enum#{}", d)
        }
        (Value::Unit, _) => String::from("()"),
        (Value::Bool(b), _) => String::from(if *b { "true" } else { "false" }),
        (Value::U64(n), _) => u64_str(*n),
        (Value::I64(n), _) => i64_str(*n),
        (Value::F64(f), _) => alloc::format!("{}", f),
        (Value::Str(s), _) => alloc::format!("\"{}\"", s),
        (Value::Bytes(b), _) => alloc::format!("<{} bytes>", b.len()),
        _ => String::from("<?>"),
    }
}
