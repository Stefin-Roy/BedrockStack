# PS/2 Keyboard Driver — Invariants

**Version:** 0.2.0
**Date:** 2026-08-01
**Source:** `kernel/src/drivers/ps2.rs`, `kernel/src/module/ps2_test.rs`, `kernel/src/acpi/madt.rs`
**Status:** Stable (x86_64 only; module is `#[cfg(target_arch = "x86_64")]`)

---

## State Invariants

**PS2-001 — Initialisation is idempotent and re-entrant-safe:**
`init()` serialises on `INIT_LOCK` and returns immediately when `PRESENT` is
already set. Only one 8042 setup sequence ever runs. Every failure path in
`do_init()` restores the original 8042 command byte before returning `false`
without setting `PRESENT`, so a later `init()` retries from a clean
controller state.
- Location: `kernel/src/drivers/ps2.rs` `init()`, `do_init()`, `restore_command_byte()`

**PS2-002 — The ISR stores only raw scancode bytes, and never locks:**
`irq_handler()` reads the 8042 data port (`0x60`) while status OBF (bit 0) is
set and pushes raw bytes into `RAW_QUEUE`, a fixed `[u8; 64]` lock-free
single-producer/single-consumer ring. Bytes flagged as AUX (status bit 5) are
discarded so mouse bytes can never be decoded as keyboard scancodes. If the
ring is full the byte is dropped and `QUEUE_OVERFLOWS` is incremented rather
than stalling the ISR. The ISR uses no locks — `SpScRing`'s head/tail atomics
are the only shared state.
- Location: `kernel/src/drivers/ps2.rs` `SpScRing`, `irq_handler()`

**PS2-003 — All scancode decoding happens outside the ISR:**
`poll_key()` pops the ring (or reads the 8042 output buffer directly when no
IRQ is wired / an edge was missed) and feeds each byte through the `Decoder`
held in a mutex. The decoder maintains the Set 1 state machine (E0/E1
prefixes, Print Screen, Pause) and the Shift/Ctrl/Alt/Super/Caps/Num/Scroll
state. The ISR never takes the decoder path.
- Location: `kernel/src/drivers/ps2.rs` `Decoder`, `decode_byte()`, `poll_key()`

**PS2-004 — The queue is a lock-free SPSC ring:**
`RAW_QUEUE` is `SpScRing`, safe to push from the ISR while the main loop pops.
The producer publishes `head` with `Release` after writing the slot; the
consumer reads `head` with `Acquire`, then publishes `tail` with `Release`;
the producer re-reads `tail` with `Acquire`. No IF manipulation is needed for
queue access, and a consumer running on another CPU cannot deadlock the ISR.
- Location: `kernel/src/drivers/ps2.rs` `SpScRing`

**PS2-005 — IRQ is wired only after the device is ready, and before IRQ is
unmasked at the 8042:**
`setup_irq()` resolves the keyboard GSI through the ACPI interrupt source
overrides (`acpi::irq_override(1)`, defaulting to GSI 1 ActiveHigh/Edge),
programs the IOAPIC entry, and registers the ISR at the exact vector the
IOAPIC assigned. Only after that does the driver set command byte bit 0
(keyboard IRQ enable) and verify it by reading the byte back. If wiring
fails, the driver falls back to polled mode.
- Location: `kernel/src/drivers/ps2.rs` `setup_irq()`, `resolve_gsi()`,
  `kernel/src/acpi/madt.rs` type-2 entries

**PS2-006 — Polled fallback is explicit and does not lose bytes:**
`poll_key()` always drains the ring first, then checks the 8042 output buffer
directly (with IF cleared for the read). `IRQ_ENABLED` distinguishes the two
modes for logging; `PRESENT` is set in either mode so a working keyboard is
never left unused.
- Location: `kernel/src/drivers/ps2.rs` `poll_key()`

**PS2-007 — Runtime device commands are serialised and isolated from the IRQ:**
`set_leds()` runs through `runtime_dev_command()`, which takes `CMD_LOCK`,
clears IF, masks the 8042 keyboard IRQ line via the command byte, flushes the
output buffer, exchanges the command byte-by-byte, flushes again, and restores
the command byte. No command response can be stolen by the ISR or interleaved
with live scancodes. `CMD_LOCK` is poll-context-only: the keyboard ISR takes
no locks, and a `debug_assert!(interrupts::are_enabled())` fires loudly if the
function is ever reached from interrupt context.
- Location: `kernel/src/drivers/ps2.rs` `runtime_dev_command()`, `set_leds()`

**PS2-008 — The test module is the last module and may never return:**
`Ps2Test` is appended last in the x86_64 `MODULES` list. If the keyboard is
present its `init()` runs a never-terminating echo loop; on the physical Esc
key it prints a message and halts forever. It returns `Ok(())` immediately
when no keyboard is present.
- Location: `kernel/src/module/ps2_test.rs`

---

## Safety Invariants

**PS2-S001 — No spin-lock deadlock between ISR and main loop:**
The ISR acquires no locks. Queue access uses the SPSC head/tail atomics; the
decoder mutex is held only by `poll_key()`. The only IF-disabled regions in
poll context are (a) the brief direct output-buffer read in `poll_key()` and
(b) `runtime_dev_command()`, neither of which is entered from an ISR.
- Location: `kernel/src/drivers/ps2.rs` `irq_handler()`, `poll_key()`, `runtime_dev_command()`

**PS2-S002 — Bounded, time-based port waits prevent hangs:**
`wait_status()` polls the status register against a TSC deadline
(`universal_timer::now_ns()`), so waits are wall-clock-bounded regardless of
CPU frequency. `flush_output()` drains at most `RAW_CAPACITY` bytes.
- Location: `kernel/src/drivers/ps2.rs` `wait_status()`, `flush_output()`

**PS2-S003 — Port I/O is confined to the driver:**
`inb`/`outb` from `platform::x86_64_pc::pit` are used only on ports `0x60` and
`0x64`. The ACPI reset path in `acpi/mod.rs` writes port `0x64` independently
but only during `reset()`, never concurrently with the driver.

**PS2-S004 — Decoder tables are immutable and bounds-checked:**
`BASE`/`SHIFTED` are `const [u8; 128]`; `handle_make`/`handle_break` reach
them only for non-extended codes already cleared of the break bit, so the
index is always `< 128`.

**PS2-S005 — `SpScRing` safety argument:**
`unsafe impl Sync for SpScRing` is justified by the SPSC protocol: the
producer writes `buf[head]` only when that slot is not reachable by the
consumer (guarded by `head`/`tail` inequality), and the consumer reads
`buf[tail]` only after observing `head` via `Acquire`. The index publishes use
`Release`, giving a happens-before edge between producer and consumer.

**PS2-S006 — IF is only disabled in poll context:**
`runtime_dev_command()` disables IF while waiting for command responses. All
its waits are pure TSC spins, so it cannot deadlock on the APIC timer wake
(which requires IF). IF is restored on every exit path, and a
`debug_assert!(interrupts::are_enabled())` on entry catches any future
in-interrupt caller before the lock is held across the disable.

---

## API Contracts

**PS2-API-001 — `pub fn init() -> bool`:**
Idempotent; returns `true` iff a keyboard was detected and configured. Must be
called from `Kernel::run()` (x86_64) after PCI/IOAPIC init and before the
module tests. Failure is non-fatal and logged.
- Location: `kernel/src/drivers/ps2.rs` `init()`, `kernel/src/lib.rs`

**PS2-API-002 — `pub fn poll_key() -> Option<KeyEvent>`:**
Non-blocking; returns at most one decoded event per call. It internally drains
the ISR ring (and, when IRQ is absent or an edge was missed, the 8042 output
buffer directly) until a full key event is produced or both sources are
exhausted — decoder-internal bytes such as `E0`/`E1` prefixes and incomplete
sequences never surface as `None` mid-stream. `KeyEvent` is `Press(Key)` or
`Release(Key)`; `Key` covers characters, F-keys, arrows, navigation,
modifiers, and keypad keys. `Key::char_repr()` yields the display character
for printable/whitespace/keypad keys. Event symmetry: modifiers and
navigation/editing/function keys report both `Press` and `Release`; lock keys
and `Pause` report only `Press`; printable characters report only `Press`.
Safe to call from the main thread with interrupts enabled.
- Location: `kernel/src/drivers/ps2.rs` `poll_key()`, `Key`, `KeyEvent`

**PS2-API-003 — `pub fn is_present() -> bool`:**
Reflects `PRESENT`; consumers (e.g. `Ps2Test`) must check this before calling
`poll_key()` so headless boots do not hang.
- Location: `kernel/src/drivers/ps2.rs` `is_present()`

**PS2-API-004 — `pub fn overflow_count() -> u64`:**
Number of scancode bytes dropped because the ring was full. Lets diagnostics
surface lost keystrokes instead of silently missing keys.
- Location: `kernel/src/drivers/ps2.rs` `overflow_count()`

**PS2-API-005 — ISR shape:**
`irq_handler` is a plain `fn()` registered via
`idt::register_device_handler_at` at the vector returned by `ioapic::enable_irq`;
EOI is sent automatically by the IDT device dispatch. The handler must not
block or take long-lived locks.
- Location: `kernel/src/drivers/ps2.rs` `irq_handler()`, `setup_irq()`,
  `kernel/src/arch/x86_64/idt.rs`

---

## Design Notes

- **Scancode set:** the driver manages the 8042 translation bit explicitly.
  It sets translation **ON** (command byte bit 6) and configures the keyboard
  to its native **Set 2**, so the controller's output is deterministic
  *translated* Set 1. `BASE`/`SHIFTED` are translated-Set-1 tables, matching
  what legacy BIOSes leave the hardware in. The decoder handles the `E0`
  prefix (arrows, navigation, right Ctrl/Alt, Super, Menu, keypad `/` and
  Enter, Print Screen) and the `E1` Pause/Break sequence.
- **Command discipline:** every device command byte is acknowledged
  individually (`send_dev_byte`); a multi-byte command only proceeds after the
  previous byte's `FA`. `0xFE` (RESEND) and controller parity/timeout errors
  trigger bounded retransmission. Reset (`FF`) reads `FA` then the BAT result
  (`AA`/`FC`), tolerating extra ID bytes and missing results.
- **LEDs:** CapsLock/NumLock/ScrollLock are mirrored to the keyboard LEDs via
  `ED`; the update is serialised and the IRQ line masked around it.
- **Key events:** lock keys are toggles — `Press` only, the physical key-up
  emits nothing; `Pause` is `Press`-only; printable characters are
  `Press`-only. Everything else reports both `Press` and `Release`.
- `BASE`/`SHIFTED` cover the full translated-Set-1 main block including
  scancode `0x2B` (`\` / `|`).
- The ISR drains the output buffer until empty (bounded by ring capacity); an
  edge-triggered IRQ that fires while bytes are pending is re-armed by the
  8042 on the next byte because reading the data port clears OBF.
- The driver is x86_64-only; `kernel/src/drivers/mod.rs` gates the module with
  `#[cfg(target_arch = "x86_64")]`. riscv64's virt machine has no i8042.
- `Ps2Test` deliberately never returns once a keyboard is present; it is the
  terminal step of `init_all()`. It uses `Console::putc_and_flush` for
  immediate per-keystroke framebuffer echo.
