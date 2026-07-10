//! Types and helpers shared between the bootloader and the kernel.
//!
//! Both crates compile for different targets (`x86_64-unknown-uefi` and
//! `x86_64-unknown-none`), but this `no_std` crate compiles for both, so the
//! boot/kernel hand-off types live here in a single place instead of being
//! duplicated (which previously risked silent ABI drift).

#![no_std]

pub mod serial;
pub mod types;

pub use types::{FramebufferInfo, MemoryRegion, MemoryRegionKind, PixelFormat};
