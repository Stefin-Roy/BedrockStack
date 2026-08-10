//! PS/2 (i8042) keyboard driver.
//!
//! # Interrupt path
//!
//! The 8042 raises the keyboard IRQ whenever a byte lands in its output
//! buffer.  The ISR drains the output buffer into a lock-free single-
//! producer/single-consumer ring (the ISR is the producer, `poll_device()` the
//! consumer); no decoding and no locks happen in interrupt context.
//!
//! # Scancode handling
//!
//! This driver explicitly manages the 8042 translation bit (command byte
//! bit 6).  It sets translation **ON** and puts the keyboard in its native
//! **Set 2**, so the controller's output is deterministic *translated*
//! Set 1.  The decoder understands `E0`-prefixed extended keys (arrows,
//! navigation, right Ctrl/Alt, Windows/Super, Menu, keypad `/` and Enter,
//! Print Screen) and the `E1`-prefixed Pause/Break sequence.
//!
//! The decoder produces **physical** key codes (`KeyCode`) plus a press/
//! release flag; it never resolves a character or a language.  Every decoded
//! event is submitted to the UInputL core (`crate::input::submit_event`) as a
//! normalized `InputEvent`, exactly like a USB HID driver would.  The keymap
//! (`crate::input::keymap::Keymap`) owns Shift/CapsLock/NumLock resolution.
//!
//! # Command discipline
//!
//! Every device command is acknowledged byte-by-byte: a command byte is
//! written to port 0x60, the host waits for `FA` (retrying on `FE`
//! RESEND), *then* any data byte is written and acknowledged the same way.
//! All waits are time-bounded (TSC-deadline) rather than iteration-counted.
//! The keyboard IRQ line and the CPU IF flag are masked around runtime
//! device commands (`ED` LED updates) so command responses can never be
//! stolen by the ISR or interleaved with live scancodes.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::drivers::serial::SerialPort;
use crate::input::event::{InputEvent, InputType};
use crate::input::keycode::KeyCode;
use crate::platform::x86_64_pc::pit::{inb, outb};
use crate::services::interrupts::InterruptManager;

const PS2_DATA: u16 = 0x60;
const PS2_CMD: u16 = 0x64;

// 8042 status register bits (read from 0x64).
const ST_OUTPUT_BUFFER_FULL: u8 = 0x01;
const ST_INPUT_BUFFER_FULL: u8 = 0x02;
const ST_AUX_OUTPUT: u8 = 0x20; // bit 5: output buffer byte came from aux port
const ST_TIMEOUT_ERR: u8 = 0x40; // bit 6: timeout error on the last transaction
const ST_PARITY_ERR: u8 = 0x80; // bit 7: parity error on the last transaction

// Controller commands (written to 0x64).
const CMD_READ_CMD_BYTE: u8 = 0x20;
const CMD_WRITE_CMD_BYTE: u8 = 0x60;
const CMD_SELF_TEST: u8 = 0xAA;
const CMD_TEST_KBD: u8 = 0xAB;
const CMD_DISABLE_KBD: u8 = 0xAD;
const CMD_ENABLE_KBD: u8 = 0xAE;
const CMD_DISABLE_AUX: u8 = 0xA7;

// 8042 command byte bits.
const CB_KBD_IRQ: u8 = 0x01; // bit 0: keyboard port IRQ enable
const CB_AUX_DISABLE: u8 = 0x20; // bit 5: aux clock disabled (set ⇒ no aux port)
const CB_TRANSLATE: u8 = 0x40; // bit 6: translate Set 2 → Set 1

// Keyboard device commands (written to 0x60).
const DEV_RESET: u8 = 0xFF;
const DEV_DISABLE_SCAN: u8 = 0xF5;
const DEV_ENABLE_SCAN: u8 = 0xF4;
const DEV_SET_SCANCODE_SET: u8 = 0xF0;
const DEV_SET_TYPEMATIC: u8 = 0xF3;
const DEV_IDENTIFY: u8 = 0xF2;
const DEV_SET_LEDS: u8 = 0xED;

const ACK: u8 = 0xFA;
const RESEND: u8 = 0xFE;
const SELF_TEST_OK: u8 = 0x55;
const INTERFACE_TEST_OK: u8 = 0x00;
const BAT_OK: u8 = 0xAA;
const BAT_ERROR: u8 = 0xFC;

const MAX_RESEND: usize = 3;

// ── Raw scancode ring buffer (lock-free SPSC) ─────────────────────

const RAW_CAPACITY: usize = 64;

/// Single-producer / single-consumer byte ring.  The ISR produces, the main
/// loop consumes.  The head/tail indexes use acquire/release so the exchange
/// is correct even if producer and consumer ever run on different CPUs; the
/// ISR never takes a lock.
struct SpScRing {
    buf: [UnsafeCell<u8>; RAW_CAPACITY],
    head: AtomicUsize, // next slot the producer writes (publishes with Release)
    tail: AtomicUsize, // next slot the consumer reads (publishes with Release)
}

// Safety: the only shared access is through the SPSC protocol enforced by the
// head/tail atomics — the producer touches `buf[head]` only when the slot is
// not reachable by the consumer, and vice versa.
unsafe impl Sync for SpScRing {}

impl SpScRing {
    const fn new() -> Self {
        SpScRing {
            buf: [const { UnsafeCell::new(0u8) }; RAW_CAPACITY],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Push a byte from the producer side.  Returns `false` (and drops the
    /// byte) if the ring is full.
    fn push(&self, byte: u8) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let next = (head + 1) % RAW_CAPACITY;
        if next == tail {
            return false;
        }
        unsafe { *self.buf[head].get() = byte };
        self.head.store(next, Ordering::Release);
        true
    }

    /// Pop a byte from the consumer side.
    fn pop(&self) -> Option<u8> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let byte = unsafe { *self.buf[tail].get() };
        self.tail.store((tail + 1) % RAW_CAPACITY, Ordering::Release);
        Some(byte)
    }
}

static RAW_QUEUE: SpScRing = SpScRing::new();
static QUEUE_OVERFLOWS: AtomicUsize = AtomicUsize::new(0);

static PRESENT: AtomicBool = AtomicBool::new(false);
static IRQ_ENABLED: AtomicBool = AtomicBool::new(false);
static IRQ_VECTOR: AtomicU8 = AtomicU8::new(0);
static LED_STATE: AtomicU8 = AtomicU8::new(0xFF); // 0xFF = never successfully sent
static LED_ERR_LOGGED: AtomicBool = AtomicBool::new(false);

static INIT_LOCK: Mutex<()> = Mutex::new(());
static CMD_LOCK: Mutex<()> = Mutex::new(());

/// The UInputL device id assigned to this keyboard during init.
static DEVICE_ID: AtomicU32 = AtomicU32::new(0);

// ── Low-level 8042 access (time-bounded) ──────────────────────────

fn status() -> u8 {
    inb(PS2_CMD)
}

/// Poll the 8042 status register until `(status & mask != 0) == want_set` or
/// `timeout_ms` elapses.  Deadlines come from the TSC clock so the wait is
/// wall-clock based, not CPU-frequency dependent.
fn wait_status(mask: u8, want_set: bool, timeout_ms: u64) -> bool {
    let deadline = crate::services::universal_timer::now_ns().saturating_add(timeout_ms * 1_000_000);
    loop {
        if (status() & mask != 0) == want_set {
            return true;
        }
        if crate::services::universal_timer::now_ns() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

fn wait_input_clear(timeout_ms: u64) -> bool {
    wait_status(ST_INPUT_BUFFER_FULL, false, timeout_ms)
}

fn wait_output(timeout_ms: u64) -> bool {
    wait_status(ST_OUTPUT_BUFFER_FULL, true, timeout_ms)
}

/// Read one byte from the 8042 data port, waiting up to 100 ms for it.
fn read_data() -> Option<u8> {
    if wait_output(100) {
        Some(inb(PS2_DATA))
    } else {
        None
    }
}

/// Drain the 8042 output buffer until empty (bounded defensively).  Returns
/// the number of bytes discarded.
fn flush_output() -> u32 {
    let mut drained = 0;
    while status() & ST_OUTPUT_BUFFER_FULL != 0 && drained < RAW_CAPACITY as u32 {
        let _ = inb(PS2_DATA);
        drained += 1;
    }
    drained
}

/// Write a controller command byte to port 0x64.
fn write_controller(cmd: u8) -> bool {
    if !wait_input_clear(50) {
        return false;
    }
    outb(PS2_CMD, cmd);
    true
}

// ── Command byte management ───────────────────────────────────────

fn read_command_byte() -> Option<u8> {
    if !write_controller(CMD_READ_CMD_BYTE) {
        return None;
    }
    read_data()
}

fn write_command_byte(cb: u8) -> bool {
    if !write_controller(CMD_WRITE_CMD_BYTE) {
        return false;
    }
    if !wait_input_clear(50) {
        return false;
    }
    outb(PS2_DATA, cb);
    true
}

/// Write the command byte and verify it by reading it back.
fn write_command_byte_verified(cb: u8) -> bool {
    if !write_command_byte(cb) {
        return false;
    }
    read_command_byte() == Some(cb)
}

fn restore_command_byte(original: u8) {
    let _ = write_command_byte(original);
}

// ── Device command layer ──────────────────────────────────────────

enum DevResp {
    Ack,
    Error(u8),
    Timeout,
}

/// Write a single byte to the keyboard and wait for its response.  A `0xFE`
/// RESEND retransmits the byte (bounded); `0xFA` is success.  This is the
/// correct per-byte acknowledgement discipline — the next byte of a multi-byte
/// command is only sent after the previous one is acknowledged.  A parity or
/// timeout error reported by the controller (status bits 6/7) also triggers a
/// retransmit, since the received byte is suspect.
fn send_dev_byte(byte: u8) -> DevResp {
    for _ in 0..MAX_RESEND {
        if !wait_input_clear(50) {
            return DevResp::Timeout;
        }
        outb(PS2_DATA, byte);
        let Some(resp) = read_data() else {
            return DevResp::Timeout;
        };
        if status() & (ST_TIMEOUT_ERR | ST_PARITY_ERR) != 0 {
            continue;
        }
        match resp {
            ACK => return DevResp::Ack,
            RESEND => continue,
            b => return DevResp::Error(b),
        }
    }
    DevResp::Error(RESEND)
}

/// Send a multi-byte device command, acknowledging each byte in sequence.
fn dev_command(bytes: &[u8]) -> bool {
    for &b in bytes {
        match send_dev_byte(b) {
            DevResp::Ack => {}
            DevResp::Error(e) => {
                SerialPort::puts("[ps2] device error 0x");
                SerialPort::put_hex(e as u64);
                SerialPort::puts(" for command byte 0x");
                SerialPort::put_hex(b as u64);
                SerialPort::puts("\n");
                return false;
            }
            DevResp::Timeout => {
                SerialPort::puts("[ps2] device response timeout\n");
                return false;
            }
        }
    }
    true
}

enum ResetOutcome {
    Ok,
    Failed,
    Timeout,
}

/// Reset the keyboard (`FF`).  `FF` is acknowledged, then the Basic Assurance
/// Test result is read: `AA` = OK, `FC` = error.  Handles RESEND retries on
/// the command itself, stray ID bytes, keyboards that emit extra bytes after
/// `AA`, and keyboards that never report a result (treated as OK).
fn reset_device() -> ResetOutcome {
    for _ in 0..MAX_RESEND {
        if !matches!(send_dev_byte(DEV_RESET), DevResp::Ack) {
            return ResetOutcome::Failed;
        }
        let deadline =
            crate::services::universal_timer::now_ns().saturating_add(1_000_000_000);
        loop {
            if crate::services::universal_timer::now_ns() >= deadline {
                return ResetOutcome::Timeout;
            }
            match read_data() {
                Some(BAT_OK) => return ResetOutcome::Ok,
                Some(BAT_ERROR) | Some(0x00) | Some(0xFD) | Some(0xFF) => {
                    return ResetOutcome::Failed;
                }
                Some(_) => continue, // stray or ID bytes following BAT
                None => continue, // nothing yet — keep waiting
            }
        }
    }
    ResetOutcome::Failed
}

/// Request the device ID (`F2`).  Returns up to two ID bytes (e.g. `AB 83`
/// AT, `AC 02` MF2, `AB 84/85` MF2 + trackpoint).  Zeroes mean "unknown".
fn identify_device() -> [u8; 2] {
    let mut id = [0u8; 2];
    if !matches!(send_dev_byte(DEV_IDENTIFY), DevResp::Ack) {
        return id;
    }
    if let Some(b) = read_data() {
        id[0] = b;
        if let Some(b) = read_data() {
            id[1] = b;
        }
    }
    id
}

/// Send a device command from poll/init context (`ED` LED updates, the
/// init-time enable-scan once the IRQ path is armed).
///
/// The keyboard IRQ line is masked via the command byte and IF is cleared so
/// no ISR can steal a command response or interleave a live scancode into the
/// shared 8042 output buffer; the buffer is flushed around the exchange and
/// the original command byte is restored.
///
/// # Constraint
///
/// `CMD_LOCK` is poll-context-only: it must never be acquired from interrupt
/// context, and the keyboard ISR takes no locks.  Combined with the
/// IF-disable below, this means the `debug_assert` fires loudly if a future
/// caller ever reaches this path with IF already cleared (which would imply an
/// in-interrupt caller holding the very lock this function takes).
fn runtime_dev_command(bytes: &[u8]) -> bool {
    let _guard = CMD_LOCK.lock();
    debug_assert!(interrupts::are_enabled(), "runtime_dev_command called from interrupt context");
    let prev_if = interrupts::are_enabled();
    interrupts::disable();
    // Drain stale bytes (possibly left by the ISR before IF was cleared)
    // so they cannot be misread as command-byte/response data.
    flush_output();
    let result = match read_command_byte() {
        Some(cb) => {
            if !write_command_byte(cb & !CB_KBD_IRQ) {
                false
            } else {
                flush_output();
                let ok = dev_command(bytes);
                flush_output();
                let _ = write_command_byte(cb);
                ok
            }
        }
        None => false,
    };
    if prev_if {
        interrupts::enable();
    }
    result
}

/// Mirror the lock-key state to the keyboard LEDs (`ED <led-byte>`).
fn set_leds(caps: bool, num: bool, scroll: bool) {
    let led = (scroll as u8) | ((num as u8) << 1) | ((caps as u8) << 2);
    if LED_STATE.load(Ordering::Relaxed) == led {
        return;
    }
    if runtime_dev_command(&[DEV_SET_LEDS, led]) {
        LED_STATE.store(led, Ordering::Relaxed);
    } else if !LED_ERR_LOGGED.swap(true, Ordering::Relaxed) {
        SerialPort::puts("[ps2] LED update not acknowledged\n");
    }
}

// ── ISR ───────────────────────────────────────────────────────────

/// Keyboard interrupt handler, invoked with interrupts disabled and EOI sent
/// by the IDT dispatch.  Drains the 8042 output buffer into the lock-free
/// ring.  Bytes flagged as AUX (mouse) are discarded so they can never be
/// decoded as keyboard scancodes.
fn irq_handler() {
    let mut drained = 0;
    while status() & ST_OUTPUT_BUFFER_FULL != 0 && drained < RAW_CAPACITY {
        let from_aux = status() & ST_AUX_OUTPUT != 0;
        let byte = inb(PS2_DATA);
        if from_aux {
            continue;
        }
        if !RAW_QUEUE.push(byte) {
            QUEUE_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            break;
        }
        drained += 1;
    }
}

// ── Scancode decoding (translated Set 1) ──────────────────────────

/// A decoded physical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decoded {
    Press(KeyCode),
    Release(KeyCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecState {
    Idle,
    GotE0,
    /// `E1` seen; expect `1D` (make) or `F0` (break) next.
    PauseFirst,
    /// Pause make: saw `E1 1D`, expect `45`.
    PauseMake45,
    /// Pause break: saw `E1 F0 1D`, expect `F0`.
    PauseBreakF0,
    /// Pause break: saw `E1 F0 1D F0`, expect `45`.
    PauseBreak45,
}

struct Decoder {
    state: DecState,
    /// `E0 2A` seen (first half of the Print Screen sequence).
    prtsc_pending: bool,
    /// Lock-key state, tracked only for the LED mirror (the keymap tracks it
    /// independently for character resolution).
    caps: bool,
    num: bool,
    scroll: bool,
}

impl Decoder {
    const fn new() -> Self {
        Decoder {
            state: DecState::Idle,
            prtsc_pending: false,
            caps: false,
            num: false,
            scroll: false,
        }
    }

    fn feed(&mut self, byte: u8) -> Option<Decoded> {
        match self.state {
            DecState::Idle => match byte {
                0xE0 => {
                    self.state = DecState::GotE0;
                    None
                }
                0xE1 => {
                    self.state = DecState::PauseFirst;
                    None
                }
                b if b & 0x80 != 0 => self.handle_break(b & 0x7F),
                b => self.handle_make(b),
            },
            DecState::GotE0 => {
                self.state = DecState::Idle;
                if byte & 0x80 != 0 {
                    self.handle_ext_break(byte & 0x7F)
                } else {
                    self.handle_ext_make(byte)
                }
            }
            DecState::PauseFirst => {
                self.state = DecState::Idle;
                match byte {
                    0x1D => self.state = DecState::PauseMake45,
                    0xF0 => self.state = DecState::PauseBreakF0,
                    _ => {}
                }
                None
            }
            DecState::PauseMake45 => {
                self.state = DecState::Idle;
                if byte == 0x45 {
                    Some(Decoded::Press(KeyCode::Pause))
                } else {
                    None
                }
            }
            DecState::PauseBreakF0 => {
                self.state = DecState::Idle;
                if byte == 0xF0 {
                    self.state = DecState::PauseBreak45;
                }
                None
            }
            DecState::PauseBreak45 => {
                self.state = DecState::Idle;
                Some(Decoded::Release(KeyCode::Pause))
            }
        }
    }

    fn handle_make(&mut self, code: u8) -> Option<Decoded> {
        self.prtsc_pending = false;
        match code {
            0x3A => {
                self.caps = !self.caps;
                self.sync_leds();
                Some(Decoded::Press(KeyCode::CapsLock))
            }
            0x45 => {
                self.num = !self.num;
                self.sync_leds();
                Some(Decoded::Press(KeyCode::NumLock))
            }
            0x46 => {
                self.scroll = !self.scroll;
                self.sync_leds();
                Some(Decoded::Press(KeyCode::ScrollLock))
            }
            _ => self.named_key(code).map(Decoded::Press),
        }
    }

    fn handle_break(&mut self, code: u8) -> Option<Decoded> {
        self.prtsc_pending = false;
        // Lock keys toggle on their make code; the physical key-up carries no
        // additional state but is still reported for uniform press/release
        // symmetry (consumers ignore it).
        self.named_key(code).map(Decoded::Release)
    }

    fn handle_ext_make(&mut self, code: u8) -> Option<Decoded> {
        match code {
            0x1D => Some(Decoded::Press(KeyCode::ControlRight)),
            0x38 => Some(Decoded::Press(KeyCode::AltRight)),
            0x5B => Some(Decoded::Press(KeyCode::SuperLeft)),
            0x5C => Some(Decoded::Press(KeyCode::SuperRight)),
            0x5D => Some(Decoded::Press(KeyCode::Menu)),
            0x2A => {
                // First half of Print Screen (`E0 2A E0 37`).
                self.prtsc_pending = true;
                None
            }
            0x37 => {
                if self.prtsc_pending {
                    self.prtsc_pending = false;
                    Some(Decoded::Press(KeyCode::PrintScreen))
                } else {
                    None
                }
            }
            0x35 => Some(Decoded::Press(KeyCode::KeypadDivide)),
            0x1C => Some(Decoded::Press(KeyCode::KeypadEnter)),
            0x48 => Some(Decoded::Press(KeyCode::ArrowUp)),
            0x50 => Some(Decoded::Press(KeyCode::ArrowDown)),
            0x4B => Some(Decoded::Press(KeyCode::ArrowLeft)),
            0x4D => Some(Decoded::Press(KeyCode::ArrowRight)),
            0x52 => Some(Decoded::Press(KeyCode::Insert)),
            0x53 => Some(Decoded::Press(KeyCode::Delete)),
            0x47 => Some(Decoded::Press(KeyCode::Home)),
            0x4F => Some(Decoded::Press(KeyCode::End)),
            0x49 => Some(Decoded::Press(KeyCode::PageUp)),
            0x51 => Some(Decoded::Press(KeyCode::PageDown)),
            _ => {
                self.prtsc_pending = false;
                None
            }
        }
    }

    fn handle_ext_break(&mut self, code: u8) -> Option<Decoded> {
        match code {
            0x1D => Some(Decoded::Release(KeyCode::ControlRight)),
            0x38 => Some(Decoded::Release(KeyCode::AltRight)),
            0x5B => Some(Decoded::Release(KeyCode::SuperLeft)),
            0x5C => Some(Decoded::Release(KeyCode::SuperRight)),
            0x5D => Some(Decoded::Release(KeyCode::Menu)),
            0x37 => {
                if self.prtsc_pending {
                    self.prtsc_pending = false;
                    Some(Decoded::Release(KeyCode::PrintScreen))
                } else {
                    None
                }
            }
            0x35 => Some(Decoded::Release(KeyCode::KeypadDivide)),
            0x1C => Some(Decoded::Release(KeyCode::KeypadEnter)),
            0x48 => Some(Decoded::Release(KeyCode::ArrowUp)),
            0x50 => Some(Decoded::Release(KeyCode::ArrowDown)),
            0x4B => Some(Decoded::Release(KeyCode::ArrowLeft)),
            0x4D => Some(Decoded::Release(KeyCode::ArrowRight)),
            0x52 => Some(Decoded::Release(KeyCode::Insert)),
            0x53 => Some(Decoded::Release(KeyCode::Delete)),
            0x47 => Some(Decoded::Release(KeyCode::Home)),
            0x4F => Some(Decoded::Release(KeyCode::End)),
            0x49 => Some(Decoded::Release(KeyCode::PageUp)),
            0x51 => Some(Decoded::Release(KeyCode::PageDown)),
            _ => {
                self.prtsc_pending = false;
                None
            }
        }
    }

    /// Physical key for a non-extended make/break code (translated Set 1).
    fn named_key(&self, code: u8) -> Option<KeyCode> {
        match code {
            0x01 => Some(KeyCode::Escape),
            0x02..=0x0D => Some(Self::digit_row(code)),
            0x0E => Some(KeyCode::Backspace),
            0x0F => Some(KeyCode::Tab),
            0x10..=0x19 => Some(Self::top_row(code)),
            0x1A => Some(KeyCode::LeftBrace),
            0x1B => Some(KeyCode::RightBrace),
            0x1C => Some(KeyCode::Enter),
            0x1D => Some(KeyCode::ControlLeft),
            0x1E..=0x26 => Some(Self::home_row(code)),
            0x27 => Some(KeyCode::Semicolon),
            0x28 => Some(KeyCode::Apostrophe),
            0x29 => Some(KeyCode::Grave),
            0x2A => Some(KeyCode::ShiftLeft),
            0x2B => Some(KeyCode::Backslash),
            0x2C..=0x32 => Some(Self::bottom_row(code)),
            0x33 => Some(KeyCode::Comma),
            0x34 => Some(KeyCode::Dot),
            0x35 => Some(KeyCode::Slash),
            0x36 => Some(KeyCode::ShiftRight),
            0x37 => Some(KeyCode::KeypadMultiply),
            0x38 => Some(KeyCode::AltLeft),
            0x39 => Some(KeyCode::Space),
            0x3A => Some(KeyCode::CapsLock),
            0x3B..=0x44 => Some(Self::fn_key(code)),
            0x45 => Some(KeyCode::NumLock),
            0x46 => Some(KeyCode::ScrollLock),
            0x47..=0x53 => Some(Self::keypad(code)),
            0x57 => Some(KeyCode::F11),
            0x58 => Some(KeyCode::F12),
            _ => None,
        }
    }

    /// Digit row: scancode `0x02..=0x0D` = Digit1..Digit0, Minus, Equal.
    fn digit_row(code: u8) -> KeyCode {
        match code {
            0x02 => KeyCode::Digit1,
            0x03 => KeyCode::Digit2,
            0x04 => KeyCode::Digit3,
            0x05 => KeyCode::Digit4,
            0x06 => KeyCode::Digit5,
            0x07 => KeyCode::Digit6,
            0x08 => KeyCode::Digit7,
            0x09 => KeyCode::Digit8,
            0x0A => KeyCode::Digit9,
            0x0B => KeyCode::Digit0,
            0x0C => KeyCode::Minus,
            0x0D => KeyCode::Equal,
            _ => KeyCode::Digit1,
        }
    }

    /// Top letter row: scancode `0x10..=0x19` = Q..P.
    fn top_row(code: u8) -> KeyCode {
        match code {
            0x10 => KeyCode::Q,
            0x11 => KeyCode::W,
            0x12 => KeyCode::E,
            0x13 => KeyCode::R,
            0x14 => KeyCode::T,
            0x15 => KeyCode::Y,
            0x16 => KeyCode::U,
            0x17 => KeyCode::I,
            0x18 => KeyCode::O,
            0x19 => KeyCode::P,
            _ => KeyCode::Q,
        }
    }

    /// Home letter row: scancode `0x1E..=0x26` = A..L.
    fn home_row(code: u8) -> KeyCode {
        match code {
            0x1E => KeyCode::A,
            0x1F => KeyCode::S,
            0x20 => KeyCode::D,
            0x21 => KeyCode::F,
            0x22 => KeyCode::G,
            0x23 => KeyCode::H,
            0x24 => KeyCode::J,
            0x25 => KeyCode::K,
            0x26 => KeyCode::L,
            _ => KeyCode::A,
        }
    }

    /// Bottom letter row: scancode `0x2C..=0x32` = Z..M.
    fn bottom_row(code: u8) -> KeyCode {
        match code {
            0x2C => KeyCode::Z,
            0x2D => KeyCode::X,
            0x2E => KeyCode::C,
            0x2F => KeyCode::V,
            0x30 => KeyCode::B,
            0x31 => KeyCode::N,
            0x32 => KeyCode::M,
            _ => KeyCode::Z,
        }
    }

    fn fn_key(code: u8) -> KeyCode {
        match code {
            0x3B => KeyCode::F1,
            0x3C => KeyCode::F2,
            0x3D => KeyCode::F3,
            0x3E => KeyCode::F4,
            0x3F => KeyCode::F5,
            0x40 => KeyCode::F6,
            0x41 => KeyCode::F7,
            0x42 => KeyCode::F8,
            0x43 => KeyCode::F9,
            0x44 => KeyCode::F10,
            0x57 => KeyCode::F11,
            0x58 => KeyCode::F12,
            _ => KeyCode::F1,
        }
    }

    /// Physical keypad key for a keypad make/break code.  NumLock resolution
    /// (digit vs. navigation) is the keymap's job, not the driver's.
    fn keypad(code: u8) -> KeyCode {
        match code {
            0x47 => KeyCode::Keypad7,
            0x48 => KeyCode::Keypad8,
            0x49 => KeyCode::Keypad9,
            0x4A => KeyCode::KeypadSubtract,
            0x4B => KeyCode::Keypad4,
            0x4C => KeyCode::Keypad5,
            0x4D => KeyCode::Keypad6,
            0x4E => KeyCode::KeypadAdd,
            0x4F => KeyCode::Keypad1,
            0x50 => KeyCode::Keypad2,
            0x51 => KeyCode::Keypad3,
            0x52 => KeyCode::Keypad0,
            0x53 => KeyCode::KeypadDecimal,
            _ => KeyCode::Keypad5,
        }
    }

    fn sync_leds(&self) {
        set_leds(self.caps, self.num, self.scroll);
    }
}

static DECODER: Mutex<Decoder> = Mutex::new(Decoder::new());

fn decode_byte(byte: u8) -> Option<Decoded> {
    DECODER.lock().feed(byte)
}

// ── UInputL producer ──────────────────────────────────────────────

/// Submit one decoded key event to the UInputL core as a normalized
/// `InputEvent` (type = Key, value = 1 pressed / 0 released).
fn submit_decoded(decoded: Decoded) {
    let id = DEVICE_ID.load(Ordering::Relaxed);
    if id == 0 {
        return;
    }
    let (code, value) = match decoded {
        Decoded::Press(code) => (code, 1),
        Decoded::Release(code) => (code, 0),
    };
    let ev = InputEvent::new(id, InputType::Key, code.code(), value);
    crate::input::submit_event(ev);
}

/// Drain the raw scancode ring (and, when no IRQ is wired, the 8042 output
/// buffer directly), decode each byte, and submit the resulting key events to
/// UInputL.  Registered as the device's `poll` hook, so `read_event` drives it
/// whenever the input queue runs dry.
pub fn poll_device() {
    loop {
        if let Some(byte) = RAW_QUEUE.pop() {
            if let Some(decoded) = decode_byte(byte) {
                submit_decoded(decoded);
            }
            continue; // prefix byte / incomplete sequence — keep draining
        }
        if PRESENT.load(Ordering::Relaxed) {
            // Clear IF while reading the output buffer directly so the ISR cannot
            // consume the same byte (racing reads would both see OBF set).
            let prev_if = interrupts::are_enabled();
            interrupts::disable();
            let byte = if status() & ST_OUTPUT_BUFFER_FULL != 0 {
                Some(inb(PS2_DATA))
            } else {
                None
            };
            if prev_if {
                interrupts::enable();
            }
            if let Some(b) = byte {
                if let Some(decoded) = decode_byte(b) {
                    submit_decoded(decoded);
                }
                continue;
            }
        }
        return;
    }
}

// ── Public API ────────────────────────────────────────────────────

/// Initialise the 8042 controller and keyboard device, then wire up the
/// keyboard interrupt and register the keyboard with UInputL.  Returns `true`
/// if a keyboard was found and configured.  Idempotent; a failed attempt
/// restores the original controller command byte so a later call retries from
/// a clean state.
pub fn init() -> bool {
    let _guard = INIT_LOCK.lock();
    if PRESENT.load(Ordering::Relaxed) {
        return true;
    }
    if do_init() {
        let id = crate::input::register_device(
            "PS/2 Keyboard",
            crate::input::CAP_KEYS,
            Some(poll_device),
        );
        DEVICE_ID.store(id, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn do_init() -> bool {
    // Drain anything the firmware left in the output buffer before touching
    // the command-byte protocol.
    flush_output();

    let Some(original_cb) = read_command_byte() else {
        SerialPort::puts("[ps2] cannot read controller command byte -- no 8042?\n");
        return false;
    };

    // Disable both interfaces so no data interleaves during configuration.
    if !write_controller(CMD_DISABLE_KBD) {
        SerialPort::puts("[ps2] controller busy (disable refused) -- keyboard unavailable\n");
        restore_command_byte(original_cb);
        return false;
    }
    let _ = write_controller(CMD_DISABLE_AUX);
    flush_output();

    // Controller self-test.
    if !write_controller(CMD_SELF_TEST) {
        SerialPort::puts("[ps2] self-test command refused -- no 8042 controller\n");
        restore_command_byte(original_cb);
        return false;
    }
    match read_data() {
        Some(SELF_TEST_OK) => SerialPort::puts("[ps2] controller self-test OK\n"),
        Some(b) => {
            SerialPort::puts("[ps2] controller self-test failed (0x");
            SerialPort::put_hex(b as u64);
            SerialPort::puts(")\n");
            restore_command_byte(original_cb);
            return false;
        }
        None => {
            SerialPort::puts("[ps2] controller self-test timeout -- no 8042 controller\n");
            restore_command_byte(original_cb);
            return false;
        }
    }

    // Keyboard interface test.
    if !write_controller(CMD_TEST_KBD) {
        SerialPort::puts("[ps2] keyboard interface test command refused\n");
        restore_command_byte(original_cb);
        return false;
    }
    match read_data() {
        Some(INTERFACE_TEST_OK) => SerialPort::puts("[ps2] keyboard interface test OK\n"),
        Some(b) => {
            SerialPort::puts("[ps2] keyboard interface test failed (0x");
            SerialPort::put_hex(b as u64);
            SerialPort::puts(")\n");
            restore_command_byte(original_cb);
            return false;
        }
        None => {
            SerialPort::puts("[ps2] keyboard interface test timeout\n");
            restore_command_byte(original_cb);
            return false;
        }
    }

    // Inspect the controller configuration (second port, translation, etc.).
    let has_aux = original_cb & CB_AUX_DISABLE == 0;
    let translation_was = original_cb & CB_TRANSLATE != 0;
    SerialPort::puts("[ps2] config byte 0x");
    SerialPort::put_hex(original_cb as u64);
    SerialPort::puts(" aux_port=");
    SerialPort::put_u64(has_aux as u64);
    SerialPort::puts(" translation_was=");
    SerialPort::put_u64(translation_was as u64);
    SerialPort::puts("\n");

    // Force translation ON so the controller converts the keyboard's native
    // Set 2 into deterministic translated Set 1; keep the keyboard IRQ line
    // masked until the device is fully configured.
    let cfg = (original_cb & !CB_KBD_IRQ) | CB_TRANSLATE;
    if !write_command_byte_verified(cfg) {
        SerialPort::puts("[ps2] failed to configure command byte (translation)\n");
        restore_command_byte(original_cb);
        return false;
    }

    // Enable the keyboard interface.
    if !write_controller(CMD_ENABLE_KBD) {
        SerialPort::puts("[ps2] failed to enable keyboard interface\n");
        restore_command_byte(original_cb);
        return false;
    }

    // Reset the keyboard device.  A flaky reset is tolerated — the
    // configure/scan commands below will catch a permanently dead keyboard.
    match reset_device() {
        ResetOutcome::Ok => SerialPort::puts("[ps2] keyboard reset OK\n"),
        ResetOutcome::Failed => SerialPort::puts("[ps2] keyboard reset failed -- continuing\n"),
        ResetOutcome::Timeout => SerialPort::puts("[ps2] keyboard reset timeout -- continuing\n"),
    }

    // Disable scanning so command responses cannot interleave with scancodes.
    if !dev_command(&[DEV_DISABLE_SCAN]) {
        SerialPort::puts("[ps2] keyboard did not ACK disable-scan -- aborting init\n");
        restore_command_byte(original_cb);
        return false;
    }

    // Identify the device.
    let id = identify_device();
    SerialPort::puts("[ps2] keyboard identified: 0x");
    SerialPort::put_hex(id[0] as u64);
    if id[1] != 0 {
        SerialPort::puts(" 0x");
        SerialPort::put_hex(id[1] as u64);
    }
    SerialPort::puts("\n");

    // Typematic: 250 ms delay, 30 Hz repeat.
    if !dev_command(&[DEV_SET_TYPEMATIC, 0x00]) {
        SerialPort::puts("[ps2] typematic config not ACKed -- continuing\n");
    }

    // Request the keyboard's native Set 2; the controller translates it to
    // Set 1 at the output.
    if !dev_command(&[DEV_SET_SCANCODE_SET, 0x02]) {
        SerialPort::puts("[ps2] scancode-set command not ACKed -- relying on default Set 2\n");
    }

    // Keep scanning OFF until the interrupt path is fully armed, so no device
    // byte can interleave with the command-byte read-back below.

    // Wire the interrupt path before unmasking the 8042's keyboard IRQ line,
    // so the first keypress after this point is caught.
    let irq_ok = setup_irq();
    flush_output(); // drain anything that arrived while the device was silent

    let mut irq_active = irq_ok;
    if irq_ok {
        let final_cb = (cfg & !CB_KBD_IRQ) | CB_KBD_IRQ;
        // Arming KBD_IRQ makes the 8042 raise IRQ1 the instant it queues the
        // CMD_READ_CMD_BYTE response, so the ISR would steal the byte and this
        // read-back would time out -- a spurious "polled mode" fallback.  Mask
        // IF so the response cannot be consumed under us (same discipline as
        // runtime_dev_command).
        let prev_if = interrupts::are_enabled();
        interrupts::disable();
        let verified = write_command_byte_verified(final_cb);
        if prev_if {
            interrupts::enable();
        }
        if verified {
            SerialPort::puts("[ps2] keyboard IRQ enabled\n");
        } else {
            SerialPort::puts("[ps2] could not enable keyboard IRQ -- polled mode\n");
            irq_active = false;
            IRQ_ENABLED.store(false, Ordering::Release);
        }
    }

    // Re-enable scanning now that the IRQ path is live.  runtime_dev_command
    // masks the IRQ line and IF around the exchange so the ACK cannot be
    // stolen by the armed ISR.
    if !runtime_dev_command(&[DEV_ENABLE_SCAN]) {
        SerialPort::puts("[ps2] keyboard did not ACK enable-scan -- aborting init\n");
        restore_command_byte(original_cb);
        return false;
    }

    PRESENT.store(true, Ordering::Release);
    SerialPort::puts("[ps2] keyboard driver ready (");
    SerialPort::puts(if irq_active { "IRQ" } else { "polled" });
    SerialPort::puts(")\n");
    true
}

/// Resolve the keyboard's legacy IRQ 1 through the ACPI interrupt source
/// overrides; defaults to GSI 1 / ActiveHigh / Edge when the MADT has no
/// override for it.
fn resolve_gsi() -> (u32, crate::acpi::Polarity, crate::acpi::TriggerMode) {
    crate::acpi::irq_override(1).unwrap_or((
        1,
        crate::acpi::Polarity::ActiveHigh,
        crate::acpi::TriggerMode::Edge,
    ))
}

/// Program the IOAPIC entry for the keyboard and register the ISR at the
/// exact vector the IOAPIC assigned.  Returns `false` (polled mode) if the
/// IOAPIC cannot route the GSI.
fn setup_irq() -> bool {
    let (gsi, polarity, trigger) = resolve_gsi();
    let Some(vector) = crate::platform::x86_64_pc::ioapic::enable_irq(gsi, polarity, trigger) else {
        SerialPort::puts("[ps2] IOAPIC routing failed -- falling back to polled mode\n");
        return false;
    };
    crate::services::x86_64::x86_interrupts::interrupts_static().register_handler(vector, irq_handler);
    IRQ_VECTOR.store(vector, Ordering::Relaxed);
    IRQ_ENABLED.store(true, Ordering::Release);
    SerialPort::puts("[ps2] keyboard IRQ wired (vector ");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts(", GSI ");
    SerialPort::put_u64(gsi as u64);
    SerialPort::puts(")\n");
    true
}

/// Whether a keyboard was successfully detected and configured during init.
pub fn is_present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Number of raw scancode bytes dropped because the driver's internal ring
/// was full.
pub fn overflow_count() -> u64 {
    QUEUE_OVERFLOWS.load(Ordering::Relaxed) as u64
}
