//! Locked serial wrapper with per-CPU re-entrancy guard and `[CPU(N)]` prefix.

use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

#[cfg(target_arch = "x86_64")]
type Inner = common::serial::x86_64::SerialPort;
#[cfg(target_arch = "riscv64")]
type Inner = common::serial::riscv64::SerialPort;

static GLOBAL_LOCK: AtomicBool = AtomicBool::new(false);
static LAST_WAS_NL: AtomicBool = AtomicBool::new(true);

/// Serial port with per-CPU re-entrancy guard and `[CPU(N)]` prefix.
///
/// Only `puts()` adds the prefix (at the start of each line).  `putc`,
/// `put_u64` and `put_hex` are raw primitives used as building blocks
/// and do NOT add a prefix.
pub struct SerialPort;

impl SerialPort {
    pub fn new() -> Self {
        Self
    }

    pub fn init() {
        Inner::init();
    }

    /// Write one raw byte without prefix.
    pub fn putc(c: u8) {
        let cpu = acquire_locks();
        Inner::putc(c);
        track_newline(c);
        release_locks(cpu);
    }

    /// Write a string, prefixing each line with `[CPU(N)] `.
    pub fn puts(s: &str) {
        let cpu = acquire_locks();

        let should_prefix = LAST_WAS_NL.load(Ordering::Relaxed);

        // If PerCpu is not initialised yet, skip prefix.
        let cpu_id = cpu.and_then(|_| crate::smp::try_current_per_cpu().map(|pc| pc.cpu_id));

        let mut need_prefix = cpu_id.is_some() && should_prefix;

        for &b in s.as_bytes() {
            if need_prefix {
                write_prefix(cpu_id.unwrap());
                need_prefix = false;
            }
            Inner::putc(b);
            if b == b'\n' {
                LAST_WAS_NL.store(true, Ordering::Relaxed);
                need_prefix = cpu_id.is_some();
            } else {
                LAST_WAS_NL.store(false, Ordering::Relaxed);
            }
        }

        release_locks(cpu);
    }

    /// Write a 64-bit value as hex without prefix.
    pub fn put_hex(val: u64) {
        let cpu = acquire_locks();
        Inner::put_hex(val);
        release_locks(cpu);
    }

    /// Write a 64-bit value in decimal without prefix.
    pub fn put_u64(val: u64) {
        let cpu = acquire_locks();
        Inner::put_u64(val);
        release_locks(cpu);
    }
}

impl core::fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Self::puts(s);
        Ok(())
    }
}

fn write_prefix(cpu_id: u32) {
    Inner::putc(b'[');
    Inner::putc(b'C');
    Inner::putc(b'P');
    Inner::putc(b'U');
    Inner::putc(b'(');
    Inner::put_u64(cpu_id as u64);
    Inner::putc(b')');
    Inner::putc(b']');
    Inner::putc(b' ');
    // These primitives don't affect LAST_WAS_NL — only the caller's content does.
}

fn track_newline(c: u8) {
    LAST_WAS_NL.store(c == b'\n', Ordering::Relaxed);
}

fn acquire_locks() -> Option<()> {
    // Per-CPU re-entrancy guard.
    if let Some(pc) = crate::smp::try_current_per_cpu() {
        while pc.serial_locked.swap(1, Ordering::Acquire) != 0 {
            core::hint::spin_loop();
        }
        compiler_fence(Ordering::SeqCst);

        while GLOBAL_LOCK.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        compiler_fence(Ordering::SeqCst);

        Some(())
    } else {
        // Before SMP init — just take the global lock.
        while GLOBAL_LOCK.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        compiler_fence(Ordering::SeqCst);
        None
    }
}

fn release_locks(cpu: Option<()>) {
    compiler_fence(Ordering::SeqCst);
    GLOBAL_LOCK.store(false, Ordering::Release);
    compiler_fence(Ordering::SeqCst);

    if cpu.is_some() {
        if let Some(pc) = crate::smp::try_current_per_cpu() {
            pc.serial_locked.store(0, Ordering::Release);
        }
    }
}
