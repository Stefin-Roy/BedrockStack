//! PS/2 (i8042) keyboard driver.
//!
//! # Interrupt path
//!
//! The 8042 raises the keyboard IRQ whenever a byte lands in its output
//! buffer.  The ISR drains the output buffer into a lock-free single-
//! producer/single-consumer ring (the ISR is the producer, `poll_key()` the
//! consumer); no decoding and no locks happen in interrupt context.
//!
//! # Scancode handling
//!
//! This driver explicitly manages the 8042 translation bit (command byte
//! bit 6).  It sets translation **ON** and puts the keyboard in its native
//! **Set 2**, so the controller's output is deterministic *translated*
//! Set 1.  The decode tables (`BASE`/`SHIFTED`) are therefore translated
//! Set 1, which is also what legacy BIOSes leave the hardware in.  The
//! decoder understands `E0`-prefixed extended keys (arrows, navigation,
//! right Ctrl/Alt, Windows/Super, Menu, keypad `/` and Enter, Print Screen)
//! and the `E1`-prefixed Pause/Break sequence, and tracks Shift / Ctrl /
//! Alt / Super / CapsLock / NumLock / ScrollLock state.  Lock states are
//! mirrored to the keyboard LEDs via `ED`.
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
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::drivers::serial::SerialPort;
use crate::platform::x86_64_pc::pit::{inb, outb};

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

// ── Public key model ──────────────────────────────────────────────

/// A single physical key, independent of any modifier state except the lock
/// keys (a keypad key resolves to either a digit or a navigation key based on
/// NumLock/Shift, mirroring the hardware behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character, already resolved through Shift/CapsLock (and
    /// Ctrl+letter → control character).
    Char(char),
    Escape,
    Backspace,
    Tab,
    Enter,
    Space,
    CapsLock,
    NumLock,
    ScrollLock,
    Shift,
    Control,
    Alt,
    AltGr,
    Super,
    Menu,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    PrintScreen,
    Pause,
    Keypad0,
    Keypad1,
    Keypad2,
    Keypad3,
    Keypad4,
    Keypad5,
    Keypad6,
    Keypad7,
    Keypad8,
    Keypad9,
    KeypadDivide,
    KeypadMultiply,
    KeypadSubtract,
    KeypadAdd,
    KeypadEnter,
    KeypadDecimal,
}

impl Key {
    /// The canonical character for printable keys: letters/digits/symbols,
    /// whitespace, and keypad digits/operators.  `None` for pure control and
    /// navigation keys.
    pub fn char_repr(self) -> Option<char> {
        match self {
            Key::Char(c) => Some(c),
            Key::Escape => Some('\x1b'),
            Key::Backspace => Some('\x08'),
            Key::Tab => Some('\t'),
            Key::Enter | Key::KeypadEnter => Some('\n'),
            Key::Space => Some(' '),
            Key::Keypad0 => Some('0'),
            Key::Keypad1 => Some('1'),
            Key::Keypad2 => Some('2'),
            Key::Keypad3 => Some('3'),
            Key::Keypad4 => Some('4'),
            Key::Keypad5 => Some('5'),
            Key::Keypad6 => Some('6'),
            Key::Keypad7 => Some('7'),
            Key::Keypad8 => Some('8'),
            Key::Keypad9 => Some('9'),
            Key::KeypadDivide => Some('/'),
            Key::KeypadMultiply => Some('*'),
            Key::KeypadSubtract => Some('-'),
            Key::KeypadAdd => Some('+'),
            Key::KeypadDecimal => Some('.'),
            _ => None,
        }
    }
}

/// A decoded keyboard event.
///
/// Event symmetry:
/// - Modifiers (`Shift`/`Control`/`Alt`/`AltGr`/`Super`) and navigation,
///   editing and function keys are reported as both `Press` and `Release`, so
///   consumers can implement chords/shortcuts and key-held semantics.
/// - Lock keys (`CapsLock`/`NumLock`/`ScrollLock`) are toggles and emit only
///   `Press` — the physical key-up carries no state change.
/// - `Pause` emits only `Press` (its break sequence is not reported).
/// - Printable characters are reported as `Press(Key::Char(..))` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Press(Key),
    Release(Key),
}

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

/// Send a device command from poll context (`ED` LED updates).
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

/// Unshifted character per make code (index = translated Set 1 scancode).
const BASE: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x01] = 0x1B; // Esc
    t[0x0E] = 0x08; // Backspace
    t[0x0F] = b'\t'; // Tab
    t[0x1C] = b'\n'; // Enter
    t[0x39] = b' '; // Space
    t[0x02] = b'1';
    t[0x03] = b'2';
    t[0x04] = b'3';
    t[0x05] = b'4';
    t[0x06] = b'5';
    t[0x07] = b'6';
    t[0x08] = b'7';
    t[0x09] = b'8';
    t[0x0A] = b'9';
    t[0x0B] = b'0';
    t[0x0C] = b'-';
    t[0x0D] = b'=';
    t[0x10] = b'q';
    t[0x11] = b'w';
    t[0x12] = b'e';
    t[0x13] = b'r';
    t[0x14] = b't';
    t[0x15] = b'y';
    t[0x16] = b'u';
    t[0x17] = b'i';
    t[0x18] = b'o';
    t[0x19] = b'p';
    t[0x1A] = b'[';
    t[0x1B] = b']';
    t[0x2B] = b'\\';
    t[0x1E] = b'a';
    t[0x1F] = b's';
    t[0x20] = b'd';
    t[0x21] = b'f';
    t[0x22] = b'g';
    t[0x23] = b'h';
    t[0x24] = b'j';
    t[0x25] = b'k';
    t[0x26] = b'l';
    t[0x27] = b';';
    t[0x28] = b'\'';
    t[0x29] = b'`';
    t[0x2C] = b'z';
    t[0x2D] = b'x';
    t[0x2E] = b'c';
    t[0x2F] = b'v';
    t[0x30] = b'b';
    t[0x31] = b'n';
    t[0x32] = b'm';
    t[0x33] = b',';
    t[0x34] = b'.';
    t[0x35] = b'/';
    t
};

/// Shifted character per make code (0 = no shifted variant).
const SHIFTED: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x02] = b'!';
    t[0x03] = b'@';
    t[0x04] = b'#';
    t[0x05] = b'$';
    t[0x06] = b'%';
    t[0x07] = b'^';
    t[0x08] = b'&';
    t[0x09] = b'*';
    t[0x0A] = b'(';
    t[0x0B] = b')';
    t[0x0C] = b'_';
    t[0x0D] = b'+';
    t[0x1A] = b'{';
    t[0x1B] = b'}';
    t[0x2B] = b'|';
    t[0x27] = b':';
    t[0x28] = b'"';
    t[0x29] = b'~';
    t[0x33] = b'<';
    t[0x34] = b'>';
    t[0x35] = b'?';
    t
};

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
    shift: bool,
    ctrl: bool,
    alt: bool,
    super_: bool,
    caps: bool,
    num: bool,
    scroll: bool,
}

impl Decoder {
    const fn new() -> Self {
        Decoder {
            state: DecState::Idle,
            prtsc_pending: false,
            shift: false,
            ctrl: false,
            alt: false,
            super_: false,
            caps: false,
            num: false,
            scroll: false,
        }
    }

    fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
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
                    Some(KeyEvent::Press(Key::Pause))
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
                None // pause released — reported as a press only
            }
        }
    }

    fn handle_make(&mut self, code: u8) -> Option<KeyEvent> {
        self.prtsc_pending = false;
        match code {
            0x2A | 0x36 => {
                self.shift = true;
                return Some(KeyEvent::Press(Key::Shift));
            }
            0x1D => {
                self.ctrl = true;
                return Some(KeyEvent::Press(Key::Control));
            }
            0x38 => {
                self.alt = true;
                return Some(KeyEvent::Press(Key::Alt));
            }
            0x3A => {
                self.caps = !self.caps;
                self.sync_leds();
                return Some(KeyEvent::Press(Key::CapsLock));
            }
            0x45 => {
                self.num = !self.num;
                self.sync_leds();
                return Some(KeyEvent::Press(Key::NumLock));
            }
            0x46 => {
                self.scroll = !self.scroll;
                self.sync_leds();
                return Some(KeyEvent::Press(Key::ScrollLock));
            }
            _ => {}
        }
        if let Some(k) = self.named_key(code) {
            return Some(KeyEvent::Press(k));
        }
        let base = BASE[code as usize];
        if base == 0 {
            return None;
        }
        // Ctrl+letter produces the classic control character (0x01..=0x1a).
        if self.ctrl && base.is_ascii_alphabetic() {
            return Some(KeyEvent::Press(Key::Char(
                (base.to_ascii_uppercase() - b'A' + 1) as char,
            )));
        }
        let shifted = SHIFTED[code as usize];
        let ch = if base.is_ascii_alphabetic() {
            if self.shift != self.caps {
                base.to_ascii_uppercase()
            } else {
                base
            }
        } else if self.shift && shifted != 0 {
            shifted
        } else {
            base
        };
        Some(KeyEvent::Press(Key::Char(ch as char)))
    }

    fn handle_break(&mut self, code: u8) -> Option<KeyEvent> {
        self.prtsc_pending = false;
        match code {
            0x2A | 0x36 => {
                self.shift = false;
                Some(KeyEvent::Release(Key::Shift))
            }
            0x1D => {
                self.ctrl = false;
                Some(KeyEvent::Release(Key::Control))
            }
            0x38 => {
                self.alt = false;
                Some(KeyEvent::Release(Key::Alt))
            }
            // Lock keys are toggles: the toggle happened on their make code and
            // the physical key-up carries no additional state, so suppress the
            // release event (see `KeyEvent` docs).
            0x3A | 0x45 | 0x46 => None,
            _ => self.named_key(code).map(KeyEvent::Release),
        }
    }

    fn handle_ext_make(&mut self, code: u8) -> Option<KeyEvent> {
        match code {
            0x1D => {
                self.ctrl = true;
                Some(KeyEvent::Press(Key::Control)) // right Ctrl
            }
            0x38 => {
                self.alt = true;
                Some(KeyEvent::Press(Key::AltGr)) // right Alt / AltGr
            }
            0x5B | 0x5C => {
                self.super_ = true;
                Some(KeyEvent::Press(Key::Super))
            }
            0x5D => Some(KeyEvent::Press(Key::Menu)),
            0x2A => {
                // First half of Print Screen (`E0 2A E0 37`).
                self.prtsc_pending = true;
                None
            }
            0x37 => {
                if self.prtsc_pending {
                    self.prtsc_pending = false;
                    Some(KeyEvent::Press(Key::PrintScreen))
                } else {
                    None
                }
            }
            0x35 => Some(KeyEvent::Press(Key::KeypadDivide)),
            0x1C => Some(KeyEvent::Press(Key::KeypadEnter)),
            0x48 => Some(KeyEvent::Press(Key::ArrowUp)),
            0x50 => Some(KeyEvent::Press(Key::ArrowDown)),
            0x4B => Some(KeyEvent::Press(Key::ArrowLeft)),
            0x4D => Some(KeyEvent::Press(Key::ArrowRight)),
            0x52 => Some(KeyEvent::Press(Key::Insert)),
            0x53 => Some(KeyEvent::Press(Key::Delete)),
            0x47 => Some(KeyEvent::Press(Key::Home)),
            0x4F => Some(KeyEvent::Press(Key::End)),
            0x49 => Some(KeyEvent::Press(Key::PageUp)),
            0x51 => Some(KeyEvent::Press(Key::PageDown)),
            _ => {
                self.prtsc_pending = false;
                None
            }
        }
    }

    fn handle_ext_break(&mut self, code: u8) -> Option<KeyEvent> {
        match code {
            0x1D => {
                self.ctrl = false;
                Some(KeyEvent::Release(Key::Control))
            }
            0x38 => {
                self.alt = false;
                Some(KeyEvent::Release(Key::AltGr))
            }
            0x5B | 0x5C => {
                self.super_ = false;
                Some(KeyEvent::Release(Key::Super))
            }
            0x5D => Some(KeyEvent::Release(Key::Menu)),
            0x37 => {
                if self.prtsc_pending {
                    self.prtsc_pending = false;
                    Some(KeyEvent::Release(Key::PrintScreen))
                } else {
                    None
                }
            }
            0x35 => Some(KeyEvent::Release(Key::KeypadDivide)),
            0x1C => Some(KeyEvent::Release(Key::KeypadEnter)),
            0x48 => Some(KeyEvent::Release(Key::ArrowUp)),
            0x50 => Some(KeyEvent::Release(Key::ArrowDown)),
            0x4B => Some(KeyEvent::Release(Key::ArrowLeft)),
            0x4D => Some(KeyEvent::Release(Key::ArrowRight)),
            0x52 => Some(KeyEvent::Release(Key::Insert)),
            0x53 => Some(KeyEvent::Release(Key::Delete)),
            0x47 => Some(KeyEvent::Release(Key::Home)),
            0x4F => Some(KeyEvent::Release(Key::End)),
            0x49 => Some(KeyEvent::Release(Key::PageUp)),
            0x51 => Some(KeyEvent::Release(Key::PageDown)),
            _ => {
                self.prtsc_pending = false;
                None
            }
        }
    }

    /// Non-modifier named key for a non-extended make/break code.
    fn named_key(&self, code: u8) -> Option<Key> {
        match code {
            0x01 => Some(Key::Escape),
            0x0E => Some(Key::Backspace),
            0x0F => Some(Key::Tab),
            0x1C => Some(Key::Enter),
            0x39 => Some(Key::Space),
            0x3A => Some(Key::CapsLock),
            0x45 => Some(Key::NumLock),
            0x46 => Some(Key::ScrollLock),
            0x3B..=0x44 => Some(Self::fn_key(code)),
            0x57 => Some(Key::F11),
            0x58 => Some(Key::F12),
            0x47..=0x53 => Some(self.keypad(code)),
            0x37 => Some(Key::KeypadMultiply),
            _ => None,
        }
    }

    fn fn_key(code: u8) -> Key {
        match code {
            0x3B => Key::F1,
            0x3C => Key::F2,
            0x3D => Key::F3,
            0x3E => Key::F4,
            0x3F => Key::F5,
            0x40 => Key::F6,
            0x41 => Key::F7,
            0x42 => Key::F8,
            0x43 => Key::F9,
            0x44 => Key::F10,
            0x57 => Key::F11,
            0x58 => Key::F12,
            _ => Key::F1,
        }
    }

    /// Resolve a keypad make/break code through NumLock (Shift inverts it,
    /// per the spec) to either a digit or a navigation key.
    fn keypad(&self, code: u8) -> Key {
        let num_active = self.num != self.shift;
        match code {
            0x47 => {
                if num_active {
                    Key::Keypad7
                } else {
                    Key::Home
                }
            }
            0x48 => {
                if num_active {
                    Key::Keypad8
                } else {
                    Key::ArrowUp
                }
            }
            0x49 => {
                if num_active {
                    Key::Keypad9
                } else {
                    Key::PageUp
                }
            }
            0x4A => Key::KeypadSubtract,
            0x4B => {
                if num_active {
                    Key::Keypad4
                } else {
                    Key::ArrowLeft
                }
            }
            0x4C => Key::Keypad5,
            0x4D => {
                if num_active {
                    Key::Keypad6
                } else {
                    Key::ArrowRight
                }
            }
            0x4E => Key::KeypadAdd,
            0x4F => {
                if num_active {
                    Key::Keypad1
                } else {
                    Key::End
                }
            }
            0x50 => {
                if num_active {
                    Key::Keypad2
                } else {
                    Key::ArrowDown
                }
            }
            0x51 => {
                if num_active {
                    Key::Keypad3
                } else {
                    Key::PageDown
                }
            }
            0x52 => {
                if num_active {
                    Key::Keypad0
                } else {
                    Key::Insert
                }
            }
            0x53 => {
                if num_active {
                    Key::KeypadDecimal
                } else {
                    Key::Delete
                }
            }
            _ => Key::Keypad5,
        }
    }

    fn sync_leds(&self) {
        set_leds(self.caps, self.num, self.scroll);
    }
}

static DECODER: Mutex<Decoder> = Mutex::new(Decoder::new());

fn decode_byte(byte: u8) -> Option<KeyEvent> {
    DECODER.lock().feed(byte)
}

// ── Public API ────────────────────────────────────────────────────

/// Initialise the 8042 controller and keyboard device, then wire up the
/// keyboard interrupt.  Returns `true` if a keyboard was found and
/// configured.  Idempotent; a failed attempt restores the original controller
/// command byte so a later call retries from a clean state.
pub fn init() -> bool {
    let _guard = INIT_LOCK.lock();
    if PRESENT.load(Ordering::Relaxed) {
        return true;
    }
    do_init()
}

fn do_init() -> bool {
    // Drain anything the firmware left in the output buffer before touching
    // the command-byte protocol.
    flush_output();

    let Some(original_cb) = read_command_byte() else {
        SerialPort::puts("[ps2] cannot read controller command byte — no 8042?\n");
        return false;
    };

    // Disable both interfaces so no data interleaves during configuration.
    if !write_controller(CMD_DISABLE_KBD) {
        SerialPort::puts("[ps2] controller busy (disable refused) — keyboard unavailable\n");
        restore_command_byte(original_cb);
        return false;
    }
    let _ = write_controller(CMD_DISABLE_AUX);
    flush_output();

    // Controller self-test.
    if !write_controller(CMD_SELF_TEST) {
        SerialPort::puts("[ps2] self-test command refused — no 8042 controller\n");
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
            SerialPort::puts("[ps2] controller self-test timeout — no 8042 controller\n");
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
        ResetOutcome::Failed => SerialPort::puts("[ps2] keyboard reset failed — continuing\n"),
        ResetOutcome::Timeout => SerialPort::puts("[ps2] keyboard reset timeout — continuing\n"),
    }

    // Disable scanning so command responses cannot interleave with scancodes.
    if !dev_command(&[DEV_DISABLE_SCAN]) {
        SerialPort::puts("[ps2] keyboard did not ACK disable-scan — aborting init\n");
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
        SerialPort::puts("[ps2] typematic config not ACKed — continuing\n");
    }

    // Request the keyboard's native Set 2; the controller translates it to
    // Set 1 at the output.
    if !dev_command(&[DEV_SET_SCANCODE_SET, 0x02]) {
        SerialPort::puts("[ps2] scancode-set command not ACKed — relying on default Set 2\n");
    }

    // Re-enable scanning.
    if !dev_command(&[DEV_ENABLE_SCAN]) {
        SerialPort::puts("[ps2] keyboard did not ACK enable-scan — aborting init\n");
        restore_command_byte(original_cb);
        return false;
    }

    // Wire the interrupt path before unmasking the 8042's keyboard IRQ line,
    // so the first keypress after this point is caught.
    let irq_ok = setup_irq();
    flush_output(); // drain anything that arrived while scanning was on

    if irq_ok {
        let final_cb = (cfg & !CB_KBD_IRQ) | CB_KBD_IRQ;
        if write_command_byte_verified(final_cb) {
            SerialPort::puts("[ps2] keyboard IRQ enabled\n");
        } else {
            SerialPort::puts("[ps2] could not enable keyboard IRQ — polled mode\n");
        }
    }

    PRESENT.store(true, Ordering::Release);
    SerialPort::puts("[ps2] keyboard driver ready (");
    SerialPort::puts(if irq_ok { "IRQ" } else { "polled" });
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
        SerialPort::puts("[ps2] IOAPIC routing failed — falling back to polled mode\n");
        return false;
    };
    crate::arch::x86_64::idt::register_device_handler_at(vector, irq_handler);
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

/// Number of scancode bytes dropped because the ring buffer was full.
pub fn overflow_count() -> u64 {
    QUEUE_OVERFLOWS.load(Ordering::Relaxed) as u64
}

/// Return the next decoded key event, if any.  Drains the ISR ring buffer
/// first; when no IRQ is wired (or an edge was missed) it also checks the
/// 8042 output buffer directly, so bytes are never stuck waiting for an edge.
///
/// Returns at most one event per call, but internally absorbs decoder-internal
/// bytes (prefix bytes, incomplete sequences) so the caller never sees a
/// `None` in the middle of a valid stream — it only gets `None` once both the
/// ring and the output buffer are exhausted.
pub fn poll_key() -> Option<KeyEvent> {
    loop {
        if let Some(byte) = RAW_QUEUE.pop() {
            if let Some(ev) = decode_byte(byte) {
                return Some(ev);
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
                if let Some(ev) = decode_byte(b) {
                    return Some(ev);
                }
                continue;
            }
        }
        return None;
    }
}
