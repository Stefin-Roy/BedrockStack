//! CPU slow-mode for the kernel — delegates to the shared guarded
//! implementation in `common` so it cannot drift from the bootloader's copy.

pub use common::cpu_slow::enable_cpu_slow_mode;
