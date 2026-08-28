//! Locked serial wrapper with per-CPU re-entrancy guard and `[CPU(N)]` prefix.
//!
//! Every byte emitted to COM1 through this wrapper is also appended to a
//! capture log (see `switch_to_growable`/`capture_bytes`), so the history of
//! all kernel serial output can be read back via `/driver/debugserial`.

use alloc::vec::Vec;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use crate::filesystems::vfs::irq::IrqMutex;

#[cfg(target_arch = "x86_64")]
type Inner = common::serial::x86_64::SerialPort;
#[cfg(target_arch = "riscv64")]
type Inner = common::serial::riscv64::SerialPort;

static GLOBAL_LOCK: AtomicBool = AtomicBool::new(false);
static LAST_WAS_NL: AtomicBool = AtomicBool::new(true);

// ── Capture log ─────────────────────────────────────────────────────────
// Serial output starts before the heap exists (kernel_main), so the first
// `CAPTURE_RING_CAP` bytes land in a static ring.  Once the heap is live,
// `switch_to_growable()` migrates the ring into a growable `Vec` and all
// subsequent bytes append there — the readback is lossless.

const CAPTURE_RING_CAP: usize = 8 * 1024;

struct CaptureLog {
    ring: [u8; CAPTURE_RING_CAP],
    ring_len: usize,
    ring_pos: usize,
    vec: Vec<u8>,
    growable: bool,
}

impl CaptureLog {
    const fn new() -> Self {
        CaptureLog {
            ring: [0; CAPTURE_RING_CAP],
            ring_len: 0,
            ring_pos: 0,
            vec: Vec::new(),
            growable: false,
        }
    }

    fn push(&mut self, c: u8) {
        if self.growable {
            self.vec.push(c);
        } else if self.ring_len == CAPTURE_RING_CAP {
            self.ring[self.ring_pos] = c;
            self.ring_pos = (self.ring_pos + 1) % CAPTURE_RING_CAP;
        } else {
            self.ring[self.ring_len] = c;
            self.ring_len += 1;
        }
    }

    fn drain_ring(&mut self) {
        if self.ring_len == 0 {
            return;
        }
        self.vec.reserve(self.ring_len);
        if self.ring_len == CAPTURE_RING_CAP && self.ring_pos > 0 {
            self.vec.extend_from_slice(&self.ring[self.ring_pos..]);
            self.vec.extend_from_slice(&self.ring[..self.ring_pos]);
        } else {
            self.vec.extend_from_slice(&self.ring[..self.ring_len]);
        }
        self.ring_len = 0;
        self.ring_pos = 0;
    }

    fn write_to(&self, out: &mut Vec<u8>) {
        if self.growable {
            out.extend_from_slice(&self.vec);
        } else if self.ring_len == CAPTURE_RING_CAP && self.ring_pos > 0 {
            out.extend_from_slice(&self.ring[self.ring_pos..]);
            out.extend_from_slice(&self.ring[..self.ring_pos]);
        } else {
            out.extend_from_slice(&self.ring[..self.ring_len]);
        }
    }
}

static CAPTURE: IrqMutex<CaptureLog> = IrqMutex::new(CaptureLog::new());

/// Migrate the pre-heap ring into the growable log.  Call once, after the
/// heap arena is live.  Idempotent.
pub fn switch_to_growable() {
    let mut log = CAPTURE.lock();
    if !log.growable {
        log.drain_ring();
        log.growable = true;
    }
}

/// Append the full captured COM1 history to `out`, oldest first.
pub fn capture_bytes(out: &mut Vec<u8>) {
    let log = CAPTURE.lock();
    log.write_to(out);
}

/// Try to dump the last `max_lines` of captured log into `w`.
/// Lock-free `try_lock`; returns `false` if `CAPTURE` is contended (no output).
/// Emits at most 1 KiB, no heap, no IRQ side effects beyond the try.
/// Used by the fault dumper and the on-screen panic screen to show the
/// 2-4 lines that immediately preceded the fault.
pub fn try_dump_last_lines<W: core::fmt::Write>(w: &mut W, max_lines: usize) -> bool {
    const MAX_BYTES: usize = 1024;
    let mut buf = [0u8; MAX_BYTES];
    let mut copy_len = 0usize;
    {
        let Some(guard) = CAPTURE.try_lock() else {
            return false;
        };
        let len = if guard.growable {
            guard.vec.len()
        } else {
            guard.ring_len
        };
        if len == 0 {
            return true;
        }
        let get = |i: usize| -> u8 {
            if guard.growable {
                guard.vec[i]
            } else if guard.ring_len == CAPTURE_RING_CAP {
                guard.ring[(guard.ring_pos + i) % CAPTURE_RING_CAP]
            } else {
                guard.ring[i]
            }
        };
        let mut k = 0usize;
        for i in 0..len {
            if get(i) == b'\n' {
                k += 1;
            }
        }
        let has_trailing = get(len - 1) != b'\n';
        let total = k + if has_trailing { 1 } else { 0 };
        let mut start = 0usize;
        if total > max_lines {
            if has_trailing {
                let target = k.saturating_sub(max_lines);
                let mut cnt = 0usize;
                for i in 0..len {
                    if get(i) == b'\n' {
                        if cnt == target {
                            start = i + 1;
                            break;
                        }
                        cnt += 1;
                    }
                }
            } else {
                // has_trailing == false, total == k
                let target = k.saturating_sub(max_lines).saturating_sub(1);
                let mut cnt = 0usize;
                for i in 0..len {
                    if get(i) == b'\n' {
                        if cnt == target {
                            start = i + 1;
                            break;
                        }
                        cnt += 1;
                    }
                }
                if k == max_lines {
                    // exact, start 0 already
                    start = 0;
                }
            }
        }
        let mut needed = len.saturating_sub(start);
        if needed > MAX_BYTES {
            let trunc = len - MAX_BYTES;
            // align to next newline to avoid half-line at start
            let mut aligned = trunc;
            for i in trunc..len {
                if get(i) == b'\n' {
                    aligned = i + 1;
                    break;
                }
            }
            if aligned != trunc && len - aligned <= MAX_BYTES {
                start = aligned;
                needed = len - start;
            } else {
                start = trunc;
                needed = MAX_BYTES;
            }
        }
        copy_len = needed.min(MAX_BYTES);
        for i in 0..copy_len {
            buf[i] = get(start + i);
        }
    }
    if copy_len == 0 {
        return true;
    }
    let s = core::str::from_utf8(&buf[..copy_len]).unwrap_or("<non-utf8 log>\n");
    let _ = w.write_str(s);
    if buf[copy_len - 1] != b'\n' {
        let _ = w.write_str("\n");
    }
    true
}

/// Record `c` into the capture log, then write it to the hardware.  This is
/// the single byte sink for all locked output paths.  The capture push never
/// calls back into serial while holding `CAPTURE`, so there is no re-entrancy.
fn emit(c: u8) {
    CAPTURE.lock().push(c);
    Inner::putc(c);
}

/// Record a batch into the capture log, then transmit it in one FIFO burst.
///
/// The capture push and the hardware write are both done once for the whole
/// slice (instead of per byte), so a multi-byte line no longer drains the TX
/// FIFO 14 times per byte.  `Inner::write_bytes` waits once per FIFO-full
/// burst, cutting both the LSR polling and the line latency.
fn emit_bytes(bytes: &[u8]) {
    {
        let mut log = CAPTURE.lock();
        for &c in bytes {
            log.push(c);
        }
    }
    Inner::write_bytes(bytes);
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
    pub fn putc(c: u8) {
        #[cfg(feature = "forceslowlogging")]
        slow_down();
        let cpu = acquire_locks();
        emit(c);
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

        // Accumulate the line (prefixes + content) into a stack buffer and
        // transmit it in one FIFO burst instead of one UART poll per byte.
        // The longest possible `[CPU(N)] ` prefix is 13 bytes.
        let mut buf = [0u8; 128];
        let mut len = 0usize;
        for &b in bytes {
            if need_prefix {
                if len + 13 > buf.len() {
                    emit_bytes(&buf[..len]);
                    len = 0;
                }
                write_prefix_into(&mut buf, &mut len, cpu_id.unwrap());
                need_prefix = false;
            }
            if len == buf.len() {
                emit_bytes(&buf);
                len = 0;
            }
            buf[len] = b;
            len += 1;
            if b == b'\n' {
                need_prefix = cpu_id.is_some();
            }
        }
        if len > 0 {
            emit_bytes(&buf[..len]);
        }

        release_locks(cpu);
    }

    /// Write a 64-bit value as hex without prefix.
    pub fn put_hex(val: u64) {
        #[cfg(feature = "forceslowlogging")]
        slow_down();
        let cpu = acquire_locks();
        write_hex(val);
        release_locks(cpu);
    }

    /// Write a 64-bit value in decimal without prefix.
    pub fn put_u64(val: u64) {
        #[cfg(feature = "forceslowlogging")]
        slow_down();
        let cpu = acquire_locks();
        write_u64(val);
        release_locks(cpu);
    }
}

fn write_hex(mut val: u64) {
    if val == 0 {
        emit(b'0');
        return;
    }
    let mut buf = [0u8; 16];
    let mut i = 16;
    while val > 0 {
        i -= 1;
        let digit = (val & 0xF) as u8;
        buf[i] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        val >>= 4;
    }
    emit_bytes(&buf[i..]);
}

fn write_u64(mut val: u64) {
    if val == 0 {
        emit(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    emit_bytes(&buf[i..]);
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
    Inner::write_bytes(s.as_bytes());
}

/// Write a u64 as hex to serial without acquiring any locks.
pub fn dump_put_hex(val: u64) {
    Inner::put_hex(val);
}

/// Write a u64 in decimal to serial without acquiring any locks.
pub fn dump_put_u64(val: u64) {
    Inner::put_u64(val);
}

/// Append `[CPU(N)] ` to a line buffer.  Caller must guarantee room for the
/// prefix (≤13 bytes: `[CPU(`, up to 3 digits, `)] `).
fn write_prefix_into(buf: &mut [u8; 128], len: &mut usize, cpu_id: u32) {
    buf[*len] = b'[';
    *len += 1;
    buf[*len] = b'C';
    *len += 1;
    buf[*len] = b'P';
    *len += 1;
    buf[*len] = b'U';
    *len += 1;
    // Decimal digits.
    let mut digits = [0u8; 4];
    let mut d = 0;
    let mut v = cpu_id as u64;
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in (0..d).rev() {
        buf[*len] = digits[i];
        *len += 1;
    }
    buf[*len] = b']';
    *len += 1;
    buf[*len] = b' ';
    *len += 1;
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
