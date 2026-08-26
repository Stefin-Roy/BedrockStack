#![no_std]
#![feature(c_variadic)]
#![feature(alloc_error_handler)]

extern crate alloc;

pub use core::ffi;

/// OOM handler for the `alloc` crate — abort the task (panic = abort is
/// already the crate profile, but an alloc failure must never unwind).
#[alloc_error_handler]
fn oom(_layout: core::alloc::Layout) -> ! {
    crate::stdlib::abort()
}

pub mod caps;
pub mod crt;
pub mod ctype;
pub mod dirent;
pub mod errno;
pub mod fb;
pub mod fd;
pub mod format;
pub mod math;
pub mod mem;
pub mod process;
pub mod scan;
pub mod signal;
pub mod stdio;
pub mod stdlib;
pub mod string;
pub mod syscall;
pub mod time;
pub mod unistd;
pub mod vfs;

/// Shared chunk size for buffered userspace file and stream I/O.
///
/// Scratch storage using this size lives in static `.bss` buffers rather than
/// on the fixed 32 KiB user stack. 1 MiB balances `fread` syscall overhead
/// for the 28 MiB `FREEDOOM.WAD` (28 vs 448 syscalls) against `.bss` cost.
pub const IO_CHUNK_BYTES: usize = 1024 * 1024;
