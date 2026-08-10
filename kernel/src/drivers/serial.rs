//! Locked serial wrapper with per-CPU line buffering and `[CPU(N)]` prefix.
//!
//! Every fragment — `puts`, `putc`, `put_u64`, `put_hex` — is appended to the
//! current CPU's line buffer, and a whole line (terminated by `\n`, or at 256
//! bytes) is flushed to the port atomically under a global lock.  This makes a
//! multi-fragment log line atomic against concurrent writers and gives each
//! line the `[CPU(N)] ` prefix of the CPU that actually produced it.

use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use core::hint::spin_loop;

use crate::smp::MAX_CPUS;

#[cfg(target_arch = "x86_64")]
type Inner = common::serial::x86_64::SerialPort;
#[cfg(target_arch = "riscv64")]
type Inner = common::serial::riscv64::SerialPort;

static GLOBAL_LOCK: AtomicBool = AtomicBool::new(false);

/// Serial port with per-CPU line buffering and `[CPU(N)]` prefix.
///
/// Only complete lines reach the port.  `putc`, `put_u64` and `put_hex` are
/// raw primitives used as building blocks and never add a prefix themselves;
/// the prefix is applied once per line by the buffering layer.
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
        raw_putc(c);
    }

    /// Write a string, prefixing each complete line with `[CPU(N)] `.
    ///
    /// The string is forwarded raw — the `raw_*` layer buffers it into the
    /// current CPU's line and applies the prefix/flush.  Nothing here touches
    /// the port directly.
    pub fn puts(s: &str) {
        raw_puts(s);
    }

    /// Write a 64-bit value as hex without prefix.
    pub fn put_hex(val: u64) {
        raw_put_hex(val);
    }

    /// Write a 64-bit value in decimal without prefix.
    pub fn put_u64(val: u64) {
        raw_put_u64(val);
    }
}

impl core::fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Self::puts(s);
        Ok(())
    }
}

// ── Per-CPU line buffers ─────────────────────────────────────────────
// Kept as a separate static (rather than fields on `PerCpu`) so the
// `#[repr(C)]` PerCpu layout — whose first field `self_ptr` is addressed via
// gs/tp — stays untouched, the same rationale as the lockdep stacks.  Each
// CPU owns exactly its own slot, indexed by `PerCpu::cpu_id`.

/// Line buffer capacity.  A full buffer flushes even without a trailing `\n`
/// so no content is ever lost or truncated.
const LINE_CAP: usize = 256;

struct LineBuf {
    data: [u8; LINE_CAP],
    len: usize,
    /// Whether the next line is a fresh one needing a `[CPU(N)] ` prefix
    /// (last flushed line ended with `\n`, or nothing flushed yet).
    at_line_start: bool,
    /// Re-entrancy guard: set while this CPU is inside an append.  A nested
    /// append (interrupt/panic in the middle of an append) bypasses the
    /// buffer and writes immediately so the buffer is never corrupted.
    appending: bool,
}

impl LineBuf {
    const fn new() -> Self {
        Self { data: [0; LINE_CAP], len: 0, at_line_start: true, appending: false }
    }
}

struct SharedLineBufs(core::cell::UnsafeCell<[LineBuf; MAX_CPUS]>);

// Each CPU only ever mutates its own slot, and re-entrancy into a slot is
// detected by `appending` before any mutation; other CPUs never touch it.
unsafe impl Sync for SharedLineBufs {}

static LINE_BUFS: SharedLineBufs = SharedLineBufs(core::cell::UnsafeCell::new([
    LineBuf::new(), LineBuf::new(), LineBuf::new(), LineBuf::new(),
    LineBuf::new(), LineBuf::new(), LineBuf::new(), LineBuf::new(),
    LineBuf::new(), LineBuf::new(), LineBuf::new(), LineBuf::new(),
    LineBuf::new(), LineBuf::new(), LineBuf::new(), LineBuf::new(),
]));

// ── Lock-guarded raw primitives ─────────────────────────────────────
// These are the serial node's implementation (services/serial.rs's
// `KernelSerial` delegates straight into them) and the pre-boot seed path.
// `SerialPort`'s public wrappers route directly here (the capability
// indirection was removed), so the whole driver collapses to raw port I/O
// under the serial locks.  They never route back through any cap.
//
// Every primitive appends to the current CPU's line buffer and flushes whole
// lines (on `\n` or buffer-full) under the serial locks, so a multi-fragment
// line is atomic against concurrent CPUs.

enum LineSlot {
    /// No per-CPU state yet (boot seed): single CPU, write straight to the
    /// port under the serial locks.
    Boot,
    /// Re-entered from within an append (interrupt/panic).  The interrupted
    /// append holds no lock while buffering, so writing to the port without
    /// taking the lock cannot deadlock; worst case the bytes interleave.
    Reentrant,
    /// The current CPU's line buffer and its `cpu_id`.
    Line(&'static mut LineBuf, u32),
}

fn line_slot() -> LineSlot {
    let Some(pc) = crate::smp::try_current_per_cpu() else {
        return LineSlot::Boot;
    };
    let cpu_id = pc.cpu_id;
    let line = unsafe { &mut (*LINE_BUFS.0.get())[(cpu_id as usize).min(MAX_CPUS - 1)] };
    if line.appending {
        LineSlot::Reentrant
    } else {
        LineSlot::Line(line, cpu_id)
    }
}

/// Write one byte to the current CPU's line buffer (no prefix).
pub(crate) fn raw_putc(c: u8) {
    match line_slot() {
        LineSlot::Boot => immediate_write(core::slice::from_ref(&c)),
        LineSlot::Reentrant => Inner::putc(c),
        LineSlot::Line(line, cpu_id) => {
            line.appending = true;
            append_byte(line, cpu_id, c);
            line.appending = false;
        }
    }
}

/// Write a string to the current CPU's line buffer.
pub(crate) fn raw_puts(s: &str) {
    match line_slot() {
        LineSlot::Boot => immediate_write(s.as_bytes()),
        LineSlot::Reentrant => {
            for &b in s.as_bytes() {
                Inner::putc(b);
            }
        }
        LineSlot::Line(line, cpu_id) => {
            line.appending = true;
            for &b in s.as_bytes() {
                append_byte(line, cpu_id, b);
            }
            line.appending = false;
        }
    }
}

/// Write a u64 as hex to the current CPU's line buffer (no prefix).
pub(crate) fn raw_put_hex(val: u64) {
    match line_slot() {
        LineSlot::Boot => immediate_hex(val),
        LineSlot::Reentrant => Inner::put_hex(val),
        LineSlot::Line(line, _) => {
            line.appending = true;
            push_hex(line, val);
            line.appending = false;
        }
    }
}

/// Write a u64 in decimal to the current CPU's line buffer (no prefix).
pub(crate) fn raw_put_u64(val: u64) {
    match line_slot() {
        LineSlot::Boot => immediate_u64(val),
        LineSlot::Reentrant => Inner::put_u64(val),
        LineSlot::Line(line, _) => {
            line.appending = true;
            push_u64(line, val);
            line.appending = false;
        }
    }
}

/// Append one byte to `line`, prefixing if it starts a fresh line and
/// flushing the whole line when a `\n` terminates it.
fn append_byte(line: &mut LineBuf, cpu_id: u32, b: u8) {
    if line.len == 0 && line.at_line_start && b != b'\n' {
        push_prefix(line, cpu_id);
    }
    push(line, b);
    if b == b'\n' {
        flush_line(line);
    }
}

/// Append one byte to `line`, flushing early if the buffer is full.
fn push(line: &mut LineBuf, b: u8) {
    line.data[line.len] = b;
    line.len += 1;
    if line.len >= LINE_CAP {
        flush_line(line);
    }
}

/// Emit `[CPU(N)] ` (N in decimal) into `line`.  Only called at line start.
fn push_prefix(line: &mut LineBuf, cpu_id: u32) {
    push(line, b'[');
    push(line, b'C');
    push(line, b'P');
    push(line, b'U');
    let mut n = cpu_id as u64;
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    if n == 0 {
        push(line, b'0');
    } else {
        while n > 0 {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        for &d in &digits[i..] {
            push(line, d);
        }
    }
    push(line, b']');
    push(line, b' ');
}

/// Write `val` as lowercase hex into `line` (no `0x`, no prefix).
fn push_hex(line: &mut LineBuf, mut val: u64) {
    if val == 0 {
        push(line, b'0');
        return;
    }
    let mut buf = [0u8; 16];
    let mut i = 16;
    while val > 0 {
        i -= 1;
        let digit = (val & 0xF) as u8;
        buf[i] = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
        val >>= 4;
    }
    for &d in &buf[i..] {
        push(line, d);
    }
}

/// Write `val` in decimal into `line` (no prefix).
fn push_u64(line: &mut LineBuf, mut val: u64) {
    if val == 0 {
        push(line, b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    for &d in &buf[i..] {
        push(line, d);
    }
}

/// Flush `line` to the port atomically under the serial locks.
fn flush_line(line: &mut LineBuf) {
    let n = line.len;
    if n == 0 {
        return;
    }
    let last_nl = line.data[n - 1] == b'\n';
    line.len = 0;
    line.at_line_start = last_nl;
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    for &b in &line.data[..n] {
        Inner::putc(b);
    }
    release_locks(cpu);
}

/// Write `bytes` straight to the port under the serial locks (boot seed).
fn immediate_write(bytes: &[u8]) {
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    for &b in bytes {
        Inner::putc(b);
    }
    release_locks(cpu);
}

/// Write a hex value straight to the port under the serial locks (boot seed).
fn immediate_hex(val: u64) {
    #[cfg(feature = "forceslowlogging")]
    slow_down();
    let cpu = acquire_locks();
    Inner::put_hex(val);
    release_locks(cpu);
}

/// Write a decimal value straight to the port under the serial locks (boot seed).
fn immediate_u64(val: u64) {
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

#[cfg(feature = "forceslowlogging")]
fn slow_down() {
    use crate::services::universal_timer;
    if universal_timer::is_ready() {
        universal_timer::sleep_ms(50);
    }
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
