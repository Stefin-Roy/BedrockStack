//! Boot/kernel hand-off types.
//!
//! These live in the shared `common` crate so the bootloader and kernel share
//! a single definition (see NOTE-05 in invariants01.md).

pub use common::types::{FramebufferInfo, MemoryRegion, MemoryRegionKind, PixelFormat};
