#![no_std]
#![feature(c_variadic)]
#![feature(core_intrinsics)]

pub mod crt;
pub mod errno;
pub mod fb;
pub mod mem;
pub mod process;
pub mod stdio;
pub mod string;
pub mod syscall;

/// Shared chunk size for buffered userspace file and stream I/O.
///
/// Scratch storage using this size lives in static `.bss` buffers rather than
/// on the fixed 32 KiB user stack.
pub const IO_CHUNK_BYTES: usize = 64 * 1024;
