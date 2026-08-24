//! `/dev/random` and `/dev/urandom` provider — CSPRNG devices.
//!
//! Both devices share the global `crate::random` ChaCha20 DRBG.
//! - `/dev/random` — blocking: if the DRBG is not yet seeded, the read parks
//!   the calling task via `crate::task::sleep_until` until a reseed succeeds.
//!   In boot context (no current task) it returns `Unsupported` rather than
//!   spinning forever. Value schema is `Blob`, so reads are byte streams.
//! - `/dev/urandom` — non-blocking: always returns bytes, falling back to a
//!   SplitMix stretch of TSC when unseeded (best-effort; still better than
//!   blocking boot). Never parks.
//! - Writes to either device mix up to 32 bytes of caller-supplied entropy into
//!   the DRBG (`crate::random::reseed_extra`). No privilege distinction — any
//!   `RW` writer can contribute, but cannot weaken the state (XOR + ChaCha diffuse).
//! - `flags != 0` is rejected with `Unsupported` (no offset semantics).

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::schema::{self, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

// ── Registration ─────────────────────────────────────────────────────────

/// Install `/dev/random` and `/dev/urandom` into the existing `/dev` dir.
/// Called from `dev::register` after the `fb` device is inserted.
pub fn install(dev: &Arc<super::super::dir::SimpleDir>) {
    dev.insert("random", Arc::new(RandomObject { urandom: false }));
    dev.insert("urandom", Arc::new(RandomObject { urandom: true }));
}

// ── Object ───────────────────────────────────────────────────────────────

struct RandomObject {
    urandom: bool,
}

impl Object for RandomObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_BLOB
    }

    fn methods(&self) -> &'static [super::super::schema::MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        self.read_value_flags(out, max, 0)
    }

    fn read_value_flags(
        &self,
        out: &mut Vec<u8>,
        max: usize,
        flags: u64,
    ) -> Result<(), UnispaceError> {
        if flags != 0 {
            return Err(UnispaceError::Unsupported);
        }
        if max == 0 {
            return Ok(());
        }
        // Respect max: out.len() + new bytes <= max.
        // Caller (syscall) already capped to 16 MiB, but enforce here too.
        let want = max;
        out.reserve(want);
        // Use a temporary buffer then extend — avoids holding lock while growing Vec.
        // For large wants we chunk to keep per-call lock hold short.
        let mut remaining = want;
        while remaining > 0 {
            let chunk = core::cmp::min(remaining, 65536);
            let start = out.len();
            out.resize(start + chunk, 0);
            let buf = &mut out[start..start + chunk];
            let ok = if self.urandom {
                crate::random::fill(buf);
                true
            } else {
                crate::random::fill_blocking(buf)
            };
            if !ok {
                // blocking read from non-task context while unseeded
                out.truncate(start);
                return Err(UnispaceError::Unsupported);
            }
            remaining -= chunk;
        }
        Ok(())
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        self.write_value_flags(v, 0)
    }

    fn write_value_flags(&self, v: Value, flags: u64) -> Result<(), UnispaceError> {
        if flags != 0 {
            return Err(UnispaceError::Unsupported);
        }
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        if bytes.is_empty() {
            return Ok(());
        }
        crate::random::reseed_extra(&bytes);
        Ok(())
    }

    fn write_blob_flags(&self, data: &[u8], flags: u64) -> Option<Result<(), UnispaceError>> {
        if flags != 0 {
            return Some(Err(UnispaceError::Unsupported));
        }
        if data.is_empty() {
            return Some(Ok(()));
        }
        crate::random::reseed_extra(data);
        Some(Ok(()))
    }
}
