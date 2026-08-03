//! Locked serial wrapper with per-CPU re-entrancy guard and `[CPU(N)]` prefix.

use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use core::hint::spin_loop;

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
        #[cfg(feature = "forceslowlogging")]
        slow_down();
        let cpu = acquire_locks();
        Inner::putc(c);
        track_newline(c);
        release_locks(cpu);
    }

    /// Write a string, prefixing each line with `[CPU(N)] `.
    pub fn puts(s: &str) {
        #[cfg(feature = "forceslowlogging")]
        slow_down();
        let cpu = acquire_locks();

        let should_prefix = LAST_WAS_NL.load(Ordering::Relaxed);

        // If PerCpu is not initialised yet, skip prefix.
        let cpu_id = cpu.and_then(|_| crate::smp::try_current_per_cpu().map(|pc| pc.cpu_id));

        let mut need_prefix = cpu_id.is_some() && should_prefix;

        let bytes = s.as_bytes();
        let has_nl = bytes.last() == Some(&b'\n');
        LAST_WAS_NL.store(has_nl, Ordering::Relaxed);

        for &b in bytes {
            if need_prefix {
                write_prefix(cpu_id.unwrap());
                need_prefix = false;
            }
            Inner::putc(b);
            if b == b'\n' {
                need_prefix = cpu_id.is_some();
            }
        }

        release_locks(cpu);
    }

    /// Write a 64-bit value as hex without prefix.
    pub fn put_hex(val: u64) {
        #[cfg(feature = "forceslowlogging")]
        slow_down();
        let cpu = acquire_locks();
        Inner::put_hex(val);
        release_locks(cpu);
    }

    /// Write a 64-bit value in decimal without prefix.
    pub fn put_u64(val: u64) {
        #[cfg(feature = "forceslowlogging")]
        slow_down();
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

// ── Lock-free raw dump output ──────────────────────────────────────
// These bypass the serial spinlock entirely.  Safe ONLY during a fault
// dump (single CPU, interrupts disabled, no concurrent access).

/// Write one byte to serial without acquiring any locks.
pub fn dump_putc(c: u8) {
    Inner::putc(c);
}

/// Write a string to serial without acquiring any locks.
pub fn dump_puts(s: &str) {
    for &b in s.as_bytes() {
        Inner::putc(b);
    }
}

/// Write a u64 as hex to serial without acquiring any locks.
pub fn dump_put_hex(val: u64) {
    Inner::put_hex(val);
}

/// Write a u64 in decimal to serial without acquiring any locks.
pub fn dump_put_u64(val: u64) {
    Inner::put_u64(val);
}

fn write_prefix(cpu_id: u32) {
    Inner::putc(b'[');
    Inner::putc(b'C');
    Inner::putc(b'P');
    Inner::putc(b'U');
    Inner::put_u64(cpu_id as u64);
    Inner::putc(b']');
    Inner::putc(b' ');
    // These primitives don't affect LAST_WAS_NL — only the caller's content does.
}

#[cfg(feature = "forceslowlogging")]
fn slow_down() {
    use crate::services::universal_timer;
    if universal_timer::is_ready() {
        universal_timer::sleep_ms(50);
    }
}

fn track_newline(c: u8) {
    LAST_WAS_NL.store(c == b'\n', Ordering::Relaxed);
}

fn acquire_locks() -> Option<()> {
    if let Some(pc) = crate::smp::try_current_per_cpu() {
        while pc.serial_locked.swap(1, Ordering::Acquire) != 0 {
            while pc.serial_locked.load(Ordering::Relaxed) != 0 {
                spin_loop();
            }
        }
        compiler_fence(Ordering::SeqCst);

        while GLOBAL_LOCK.swap(true, Ordering::Acquire) {
            while GLOBAL_LOCK.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        compiler_fence(Ordering::SeqCst);

        Some(())
    } else {
        while GLOBAL_LOCK.swap(true, Ordering::Acquire) {
            while GLOBAL_LOCK.load(Ordering::Relaxed) {
                spin_loop();
            }
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
