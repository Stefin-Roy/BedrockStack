use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::dir::SimpleDir;
use super::super::schema::{self, Field, MethodDesc, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

use crate::drivers::serial::SerialPort;

/// Register the `/driver` system (kernel driver introspection objects).
pub fn register() -> Result<(), UnispaceError> {
    let driver = Arc::new(SimpleDir::new());
    driver.insert("debugserial", Arc::new(DebugSerialObject));
    #[cfg(target_arch = "x86_64")]
    driver.insert("audio", Arc::new(AudioObject));
    super::super::register("driver", driver)
}

/// `/driver/debugserial`: read returns the full captured COM1 history,
/// write appends the payload to COM1 (and therefore to the capture log too).
struct DebugSerialObject;

impl Object for DebugSerialObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_BLOB
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        crate::drivers::serial::capture_bytes(out);
        out.truncate(core::cmp::min(max, out.len()));
        Ok(())
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        for &c in &bytes {
            SerialPort::putc(c);
        }
        Ok(())
    }
}

/// `/driver/audio` — the audio subsystem's device surface.
///
/// The value is a snapshot of the live playback/capture state.  The two
/// methods drive the existing blocking engine: `:play_tone{freq, ms}` and
/// `:play_pcm{pcm}` (little-endian interleaved 16-bit signed stereo, 48 kHz).
/// Playback occupies the single hardware device for its whole duration and
/// HLTs the calling CPU (the engine's blocking waits yield via HLT), so this
/// surface is strictly single-consumer.  x86_64-only: the `audio` crate
/// module is not compiled on riscv64.
#[cfg(target_arch = "x86_64")]
struct AudioObject;

/// `read(/driver/audio)`: snapshot of the live audio device, or
/// `{present:false, name:""}` when no controller initialised.
#[cfg(target_arch = "x86_64")]
pub static AUDIO_STATE: Schema = Schema::Struct(&[
    Field { name: "present", ty: &schema::SCHEMA_BOOL },
    Field { name: "name", ty: &schema::SCHEMA_STR },
    Field { name: "sample_rate", ty: &schema::SCHEMA_U32 },
    Field { name: "channels", ty: &schema::SCHEMA_U32 },
    Field { name: "can_record", ty: &schema::SCHEMA_BOOL },
]);

/// `write(/driver/audio:play_tone, {freq, ms})`.
#[cfg(target_arch = "x86_64")]
static PLAY_TONE_IN: Schema = Schema::Struct(&[
    Field { name: "freq", ty: &schema::SCHEMA_U32 },
    Field { name: "ms", ty: &schema::SCHEMA_U64 },
]);

/// `write(/driver/audio:play_pcm, {pcm})` — raw little-endian interleaved
/// `i16` stereo samples at 48 kHz.
#[cfg(target_arch = "x86_64")]
static PLAY_PCM_IN: Schema = Schema::Struct(&[Field {
    name: "pcm",
    ty: &schema::SCHEMA_BYTES,
}]);

/// Playback staging buffer limit (see `hda.rs`): 256 KiB.
#[cfg(target_arch = "x86_64")]
const PLAY_STAGING_LIMIT: usize = 0x40000;

#[cfg(target_arch = "x86_64")]
static AUDIO_METHODS: [MethodDesc; 2] = [
    MethodDesc { name: "play_tone", input: &PLAY_TONE_IN, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "play_pcm", input: &PLAY_PCM_IN, output: &schema::SCHEMA_UNIT },
];

#[cfg(target_arch = "x86_64")]
impl Object for AudioObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &AUDIO_STATE
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &AUDIO_METHODS
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::Struct(vec![
            Value::Bool(crate::audio::is_ready()),
            Value::Str(alloc::string::String::from(
                crate::audio::device_name().unwrap_or(""),
            )),
            Value::U64(crate::audio::SAMPLE_RATE as u64),
            Value::U64(crate::audio::CHANNELS as u64),
            Value::Bool(crate::audio::can_record()),
        ]);
        schema::encode_value(&v, &AUDIO_STATE, out)
    }

    fn invoke(&self, method: usize, v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        if !crate::audio::is_ready() {
            // No device: never block or synthesise from an absent engine.
            return Err(UnispaceError::Unsupported);
        }
        match method {
            0 => {
                let fields = match v {
                    Value::Struct(f) => f,
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                let freq = match fields.first() {
                    Some(Value::U64(n)) => *n as u32,
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                let ms = match fields.get(1) {
                    Some(Value::U64(n)) => *n,
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                crate::audio::play_tone(freq, ms).map_err(unsupported)?;
                Ok(())
            }
            1 => {
                let fields = match v {
                    Value::Struct(f) => f,
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                let pcm = match fields.into_iter().next() {
                    Some(Value::Bytes(b)) => b,
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                if pcm.is_empty() || pcm.len() % 2 != 0 || pcm.len() > PLAY_STAGING_LIMIT {
                    return Err(UnispaceError::InvalidArgument);
                }
                let mut samples = alloc::vec::Vec::with_capacity(pcm.len() / 2);
                for pair in pcm.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([pair[0], pair[1]]));
                }
                crate::audio::play_pcm(&samples).map_err(unsupported)?;
                Ok(())
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}

/// Map a driver error string to a namespace error: absent device, over-size
/// sample, or a playback failure all surface as `Unsupported` (no errno
/// vocabulary for `&'static str` failures exists on the wire).
#[cfg(target_arch = "x86_64")]
fn unsupported(_e: &'static str) -> UnispaceError {
    UnispaceError::Unsupported
}
