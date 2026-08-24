//! Random kernel service — CSPRNG for in-kernel consumers.
//!
//! Wraps `crate::random` global ChaCha20 DRBG as a capability-style service
//! (`KernelServices::random`). Drivers and other kernel subsystems should prefer
//! this service over directly calling `crate::random` when they already hold
//! `KernelServices`, so the dependency is explicit and mockable.

/// CSPRNG service for kernel-internal use.
pub trait Random: Send + Sync {
    /// Fill `buf` with cryptographically secure random bytes.
    /// Never blocks; falls back to SplitMix stretch if not yet seeded.
    fn fill(&self, buf: &mut [u8]);

    /// Return a random `u64`.
    fn fill_u64(&self) -> u64;

    /// Return a random `u32`.
    fn fill_u32(&self) -> u32;

    /// Mix `extra` entropy into the DRBG (reseed).
    fn reseed(&self, extra: &[u8]);

    /// True if the DRBG has been seeded with real entropy.
    fn is_seeded(&self) -> bool;
}

pub struct GlobalRandom;

impl Random for GlobalRandom {
    fn fill(&self, buf: &mut [u8]) {
        crate::random::fill(buf);
    }

    fn fill_u64(&self) -> u64 {
        crate::random::random_u64()
    }

    fn fill_u32(&self) -> u32 {
        crate::random::random_u32()
    }

    fn reseed(&self, extra: &[u8]) {
        crate::random::reseed_extra(extra);
    }

    fn is_seeded(&self) -> bool {
        crate::random::is_seeded()
    }
}

static GLOBAL_RANDOM: GlobalRandom = GlobalRandom;

/// Initialise the random service. The underlying DRBG must already be seeded
/// via `crate::random::init()` (called from `Kernel::init` before services).
pub fn init() -> &'static dyn Random {
    &GLOBAL_RANDOM
}
