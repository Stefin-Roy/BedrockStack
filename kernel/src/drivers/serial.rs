//! COM1 serial driver.
//!
//! Re-exported from the shared `common` crate so the bootloader and kernel
//! share a single implementation.

pub use common::serial::SerialPort;
