//! Locked serial wrapper with per-CPU re-entrancy guard and `[CPU(N)]` prefix.

use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use core::hint::spin_loop;

#[cfg(target_arch = "x86_64")]
type Inner = common::serial::x86_64::SerialPort;
#[cfg(target_arch = "riscv64")]
type Inner = common::serial::riscv64::SerialPort;

static GLOBAL_LOCK: AtomicBool = AtomicBool::new(false);
static LAST_WAS_NL: AtomicBool = AtomicBool::new(true);

// ── Capability-edge routing ───────────────────────────────────────────
// Once the capability graph exists, console output crosses the serial cap
// (SERIAL_PUTS / SERIAL_PUTC) instead of touching the port ambiently.  The
// port itself is written only by the raw primitives below — the serial
// node's implementation (services/serial.rs) and the pre-boot seed path.
//
// `SINK_ARMED` flips once `obj::bootstrap::bootstrapped()` becomes true;
// `IN_CAP` is the re-entrancy guard (nested logging inside a cap dispatch
// falls back to the raw path); `SINK` caches the boot-domain `SerialClient`
// so the capability table is resolved once, not per line.
static SINK_ARMED: AtomicBool = AtomicBool::new(false);
static IN_CAP: AtomicBool = AtomicBool::new(false);
static SINK: spin::Once<crate::obj::clients::SerialClient> = spin::Once::new();

/// Arm the capability sink once bootstrap has completed.  Safe to call
/// repeatedly; no-ops until `obj::bootstrap::bootstrapped()` is true.
pub fn arm_cap_sink() {
    if crate::obj::bootstrap::bootstrapped() {
        SINK_ARMED.store(true, Ordering::Relaxed);
    }
}

/// True once the capability sink may be used.  Arms lazily on the first
/// post-bootstrap write, so no explicit handshake is required.
fn cap_sink_ready() -> bool {
    if !SINK_ARMED.load(Ordering::Relaxed) {
        arm_cap_sink();
    }
    SINK_ARMED.load(Ordering::Relaxed)
}

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
    ///
    /// Post-bootstrap the byte crosses the serial capability; before that
    /// (boot seed / re-entrancy) it goes straight to the port.
    pub fn putc(c: u8) {
        if !cap_sink_ready() || IN_CAP.load(Ordering::Relaxed) {
            raw_putc(c);
            return;
        }
        IN_CAP.store(true, Ordering::Relaxed);
        let client = SINK.call_once(crate::obj::clients::SerialClient::boot_serial);
        client.putc(c);
        IN_CAP.store(false, Ordering::Relaxed);
    }

    /// Write a string, prefixing each line with `[CPU(N)] `.
    ///
    /// The prefix/line logic stays here; on the raw path it streams straight
    /// to the port, on the cap path it forwards the whole (already-prefixed)
    /// line through the serial capability.
    pub fn puts(s: &str) {
        if !cap_sink_ready() || IN_CAP.load(Ordering::Relaxed) {
            // Pre-bootstrap seed / re-entrancy: prefix and write to the port
            // under the serial locks.
            let cpu = acquire_locks();
            let cpu_id = cpu.and_then(|_| crate::smp::try_current_per_cpu().map(|pc| pc.cpu_id));
            write_line(s, cpu_id, &mut |b| Inner::putc(b));
            release_locks(cpu);
            return;
        }
        // Capability path: apply the `[CPU(N)] ` prefix here, then forward the
        // whole already-prefixed line through the cap.  The node's `raw_puts`
        // writes it verbatim, so there is no re-prefixing.
        let cpu_id = crate::smp::try_current_per_cpu().map(|pc| pc.cpu_id);
        let mut line: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(s.len() + 16);
        write_line(s, cpu_id, &mut |b| line.push(b));
        IN_CAP.store(true, Ordering::Relaxed);
        let client = SINK.call_once(crate::obj::clients::SerialClient::boot_serial);
        client.puts(core::str::from_utf8(&line).unwrap_or(s));
        IN_CAP.store(false, Ordering::Relaxed);
    }

    /// Write a 64-bit value as hex without prefix.
    pub fn put_hex(val: u64) {
        if !cap_sink_ready() || IN_CAP.load(Ordering::Relaxed) {
            raw_put_hex(val);
            return;
        }
        IN_CAP.store(true, Ordering::Relaxed);
        let client = SINK.call_once(crate::obj::clients::SerialClient::boot_serial);
        client.put_hex(val);
        IN_CAP.store(false, Ordering::Relaxed);
    }

    /// Write a 64-bit value in decimal without prefix.
    pub fn put_u64(val: u64) {
        if !cap_sink_ready() || IN_CAP.load(Ordering::Relaxed) {
            raw_put_u64(val);
            return;
        }
        IN_CAP.store(true, Ordering::Relaxed);
        let client = SINK.call_once(crate::obj::clients::SerialClient::boot_serial);
        client.put_u64(val);
        IN_CAP.store(false, Ordering::Relaxed);
    }
}

impl core::fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Self::puts(s);
        Ok(())
    }
}

// ── Lock-guarded raw primitives ─────────────────────────────────────
// These are the serial node's implementation (services/serial.rs dispatches
// into them through the serial capability) and the pre-boot seed path.  They
// are the only place raw port I/O happens post-bootstrap.  They never route
// back through the cap, so calling them from inside cap dispatch is safe.
//
// `raw_puts` writes its input verbatim — the cap path forwards an
// already-prefixed line, so no re-prefixing happens here.

/// Write one byte to the port under the serial locks (no prefix).
pub(crate) fn raw_putc(c: u8) {
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    Inner::putc(c);
    track_newline(c);
    release_locks(cpu);
}

/// Write a string to the port under the serial locks (no prefix).
pub(crate) fn raw_puts(s: &str) {
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    for &b in s.as_bytes() {
        Inner::putc(b);
    }
    release_locks(cpu);
}

/// Write a u64 as hex to the port under the serial locks (no prefix).
pub(crate) fn raw_put_hex(val: u64) {
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    Inner::put_hex(val);
    release_locks(cpu);
}

/// Write a u64 in decimal to the port under the serial locks (no prefix).
pub(crate) fn raw_put_u64(val: u64) {
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    Inner::put_u64(val);
    release_locks(cpu);
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

/// Emit `[CPU(N)] ` to `sink` (N in decimal).  These bytes don't affect
/// `LAST_WAS_NL` — only the caller's content does.
fn write_prefix<F: FnMut(u8)>(cpu_id: u32, sink: &mut F) {
    sink(b'[');
    sink(b'C');
    sink(b'P');
    sink(b'U');
    let mut n = cpu_id as u64;
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    if n == 0 {
        sink(b'0');
    } else {
        while n > 0 {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        for &d in &digits[i..] {
            sink(d);
        }
    }
    sink(b']');
    sink(b' ');
}

/// The `[CPU(N)] ` line-prefix logic, streaming through a byte sink so the
/// same walker drives both the raw path (`Inner`) and the cap path (`Vec`).
fn write_line<F: FnMut(u8)>(s: &str, cpu_id: Option<u32>, sink: &mut F) {
    let should_prefix = LAST_WAS_NL.load(Ordering::Relaxed);

    let mut need_prefix = cpu_id.is_some() && should_prefix;

    let bytes = s.as_bytes();
    let has_nl = bytes.last() == Some(&b'\n');
    LAST_WAS_NL.store(has_nl, Ordering::Relaxed);

    for &b in bytes {
        if need_prefix {
            write_prefix(cpu_id.unwrap(), sink);
            need_prefix = false;
        }
        sink(b);
        if b == b'\n' {
            need_prefix = cpu_id.is_some();
        }
    }
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
