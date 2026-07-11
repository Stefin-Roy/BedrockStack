//! COM1 serial driver — delegates to the current arch's I/O backend.

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::serial::SerialPort;
#[cfg(target_arch = "riscv64")]
pub use crate::arch::riscv64::serial::SerialPort;
