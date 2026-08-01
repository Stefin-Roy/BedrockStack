# PS/2 Keyboard Driver — Invariants

**Version:** 0.4.0
**Date:** 2026-08-01
**Source:** `kernel/src/drivers/ps2.rs`, `kernel/src/input/{keycode,event}.rs`, `kernel/src/acpi/madt.rs`
**Status:** Stable (x86_64 only; module is `#[cfg(target_arch = "x86_64")]`)

---

## State Invariants

**PS2-001 — Initialisation is idempotent and re-entrant-safe:**
`init()` serialises on `INIT_LOCK` and returns immediately when `PRESENT` is
already set. Only one 8042 setup sequence ever runs. Every failure path in
`do_init()` restores the original 8042 command byte before returning `false`
without setting `PRESENT`, so a later `init()` retries from a clean
controller state. `register_device` is called only once, on the success path.
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
`poll_device()` pops the ring (or reads the 8042 output buffer directly when
no IRQ is wired / an edge was missed) and feeds each byte through the
`Decoder` held in a mutex. The decoder maintains the Set 1 state machine (E0/E1
prefixes, Print Screen, Pause) and the lock-key state used for the LED mirror.
The ISR never takes the decoder path. `poll_device()` is registered as the
UInputL device's `poll` hook, so `input::read_event()` drives it whenever the
input queue runs dry.
- Location: `kernel/src/drivers/ps2.rs` `Decoder`, `decode_byte()`, `poll_device()`

**PS2-004 — The raw queue is a lock-free SPSC ring:**
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
`poll_device()` always drains the ring first, then checks the 8042 output buffer
directly (with IF cleared for the read). `IRQ_ENABLED` distinguishes the two
modes for logging; `PRESENT` is set in either mode so a working keyboard is
never left unused.
- Location: `kernel/src/drivers/ps2.rs` `poll_device()`

**PS2-007 — Runtime device commands are serialised and isolated from the IRQ:**
`set_leds()` runs through `runtime_dev_command()`, which takes `CMD_LOCK`,
clears IF, masks the 8042 keyboard IRQ line via the command byte, flushes the
output buffer, exchanges the command byte-by-byte, flushes again, and restores
the command byte. No command response can be stolen by the ISR or interleaved
with live scancodes. `CMD_LOCK` is poll-context-only: the keyboard ISR takes
no locks, and a `debug_assert!(interrupts::are_enabled())` fires loudly if the
function is ever reached from interrupt context.
- Location: `kernel/src/drivers/ps2.rs` `runtime_dev_command()`, `set_leds()`

**PS2-008 — The driver is a pure producer; it never buffers for a consumer:**
Decoded keys are immediately wrapped as normalized `InputEvent`s
(`submit_decoded`) and pushed into the UInputL queue via
`input::submit_event`. The driver holds no consumer state, no echo logic, and
no character tables — layout resolution (including NumLock semantics) is the
keymap's job in `kernel/src/input/keymap.rs`.
- Location: `kernel/src/drivers/ps2.rs` `submit_decoded()`,
  `kernel/src/input/keymap.rs`

**PS2-009 — The test module is the last module and may never return:**
`InputTest` is appended last in the x86_64 `MODULES` list. If any input device
is present its `init()` runs a never-terminating echo loop; on the physical
Esc key it prints a message and halts forever. It returns `Ok(())` immediately
when no input device is present.
- Location: `kernel/src/module/input_test.rs`

---

## Safety Invariants

**PS2-S001 — No spin-lock deadlock between ISR and main loop:**
The ISR acquires no locks. Queue access uses the SPSC head/tail atomics; the
decoder mutex is held only by `poll_device()`. The only IF-disabled regions in
poll context are (a) the brief direct output-buffer read in `poll_device()`
and (b) `runtime_dev_command()`, neither of which is entered from an ISR.
- Location: `kernel/src/drivers/ps2.rs` `irq_handler()`, `poll_device()`, `runtime_dev_command()`

**PS2-S002 — Bounded, time-based port waits prevent hangs:**
`wait_status()` polls the status register against a TSC deadline
(`universal_timer::now_ns()`), so waits are wall-clock-bounded regardless of
CPU frequency. `flush_output()` drains at most `RAW_CAPACITY` bytes.
- Location: `kernel/src/drivers/ps2.rs` `wait_status()`, `flush_output()`

**PS2-S003 — Port I/O is confined to the driver:**
`inb`/`outb` from `platform::x86_64_pc::pit` are used only on ports `0x60` and
`0x64`. The ACPI reset path in `acpi/mod.rs` writes port `0x64` independently
but only during `reset()`, never concurrently with the driver.

**PS2-S004 — Decoder key tables are immutable and bounds-checked:**
`named_key()`, `digit_row()`, `top_row()`, `home_row()`, `bottom_row()`,
`fn_key()` and `keypad()` are pure matches over translated-Set-1 scancode
ranges covering the full main block (digit row `0x02–0x0D`, letter rows
`0x10–0x19`, `0x1E–0x26`, `0x2C–0x32`, punctuation, modifiers, locks, F-keys,
keypad); unknown codes return `None` and are dropped, never mis-translated.
Lock-key state (`caps`/`num`/`scroll`) in the decoder exists only to mirror the
keyboard LEDs; character resolution never reads it.

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
called from `Kernel::run()` (x86_64) after `input::init()` and after PCI/IOAPIC
init, before the module tests. Failure is non-fatal and logged. On success it
registers the device with UInputL and stores the returned id in `DEVICE_ID`.
- Location: `kernel/src/drivers/ps2.rs` `init()`, `kernel/src/lib.rs`

**PS2-API-002 — `pub fn poll_device()`:**
Registered as the UInputL device `poll` hook. Non-blocking; drains the raw
ring (and, when IRQ is absent or an edge was missed, the 8042 output buffer
directly) until empty, decoding each byte and submitting normalized
`InputEvent`s. Decoder-internal bytes such as `E0`/`E1` prefixes and
incomplete sequences never surface as events. Safe to call from the main
thread with interrupts enabled.
- Location: `kernel/src/drivers/ps2.rs` `poll_device()`, `submit_decoded()`

**PS2-API-003 — `pub fn is_present() -> bool`:**
Reflects `PRESENT`; consumers (e.g. `InputTest`) must check this (or
`input::device_count()`) before assuming keystrokes are available so headless
boots do not hang.
- Location: `kernel/src/drivers/ps2.rs` `is_present()`

**PS2-API-004 — `pub fn overflow_count() -> u64`:**
Number of raw scancode bytes dropped because the ring was full. Lets
diagnostics surface lost keystrokes instead of silently missing keys.
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
  *translated* Set 1. The decoder handles the `E0` prefix (arrows, navigation,
  right Ctrl/Alt, Super, Menu, keypad `/` and Enter, Print Screen) and the `E1`
  Pause/Break sequence.
- **Command discipline:** every device command byte is acknowledged
  individually (`send_dev_byte`); a multi-byte command only proceeds after the
  previous byte's `FA`. `0xFE` (RESEND) and controller parity/timeout errors
  trigger bounded retransmission. Reset (`FF`) reads `FA` then the BAT result
  (`AA`/`FC`), tolerating extra ID bytes and missing results.
- **LEDs:** CapsLock/NumLock/ScrollLock are mirrored to the keyboard LEDs via
  `ED`; the update is serialised and the IRQ line masked around it. The decoder
  toggles its own lock-key state purely for this mirror — the UInputL keymap
  tracks the same state independently for character resolution.
- **Physical keys only:** the decoder emits `Decoded::Press(KeyCode)` /
  `Decoded::Release(KeyCode)`. Lock keys toggle on their make code (the break
  is still reported for press/release symmetry) and `Pause` is `Press`-only.
  Nothing in the driver encodes characters or layout.
- **UInputL contract:** the driver registers itself with
  `input::register_device("PS/2 Keyboard", CAP_KEYS, Some(poll_device))` and
  submits only via `input::submit_event`. It never reads from the input queue.
- The ISR drains the output buffer until empty (bounded by ring capacity); an
  edge-triggered IRQ that fires while bytes are pending is re-armed by the
  8042 on the next byte because reading the data port clears OBF.
- The driver is x86_64-only; `kernel/src/drivers/mod.rs` gates the module with
  `#[cfg(target_arch = "x86_64")]`. riscv64's virt machine has no i8042.
- `InputTest` deliberately never returns once an input device is present; it is
  the terminal step of `init_all()`. It uses `Console` methods (`clear`,
  `putc_and_flush`, `backspace`, `delete`) for per-keystroke framebuffer echo.
