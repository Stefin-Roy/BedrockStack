# UInputL — Unified Input Layer — Invariants

**Version:** 0.3.0
**Date:** 2026-08-03
**Source:** `kernel/src/input/{mod,event,keycode,queue,keymap}.rs`, `kernel/src/drivers/ps2.rs`
**Status:** Stable (arch-independent core; x86_64 consumer today via PS/2)

---

## State Invariants

**INPUT-001 — Drivers are producers; consumers read; the core routes:**
Hardware drivers never deliver input directly to a consumer. They register a
device (`register_device`) and submit normalized `InputEvent`s
(`submit_event`). Consumers obtain events via `read_event` or register a
`subscribe` handler. The core owns timestamps, device ids, the event queue,
grab/focus and subscriber dispatch.
- Location: `kernel/src/input/mod.rs`

**INPUT-002 — The core is initialised before any driver registers:**
`input::init()` is called from `Kernel::run()` before any driver registers or
submits; the event queue is a plain `static const` (no heap, no runtime
builder), so init is a no-op for state setup but still the ordering
contract. `Kernel::run()` calls `input::init()` before `ps2::init()`.
- Location: `kernel/src/input/mod.rs` `init()`, `queue()`; `kernel/src/lib.rs`
**INPUT-003 — The event queue is a bounded lock-free MPSC ring:**
`InputQueue` uses the Vyukov algorithm with per-slot **round counters**: a
stream position `pos` maps to slot `pos % CAPACITY` in round `pos / CAPACITY`
(`CAPACITY` = 256). All slots start at `seq == 0` ("round 0, empty"), so the
queue is a `static const`. `push` CAS-claims `head`, waits for
`seq == 2*round`, writes, publishes `seq = 2*round + 1`; the single consumer
waits for `seq == 2*round + 1`, reads, then releases the slot with
`seq = 2*round + 2`. A full queue makes `push` return `false` (caller drops)
instead of spinning. `pop` is single-consumer and needs no CAS on `tail`.
`len()` is approximate.
- Location: `kernel/src/input/queue.rs`

**INPUT-004 — A full queue never stalls a driver:**
`submit_event` returns `false` and bumps `OVERFLOWS` when the queue is full.
The ISR path (PS/2 `submit_decoded`) tolerates the drop. `overflow_count()`
surfaces drops for diagnostics.
- Location: `kernel/src/input/mod.rs` `submit_event()`, `overflow_count()`

**INPUT-005 — The core stamps timestamps:**
`submit_event` overwrites the event timestamp with
`universal_timer::now_ns()` regardless of what the driver wrote, so a driver
never needs a clock and event ordering is monotonic by enqueue time.
- Location: `kernel/src/input/mod.rs` `submit_event()`,
  `kernel/src/services/universal_timer.rs`

**INPUT-006 — Device ids are core-owned and unique:**
`register_device` allocates the next id from `NEXT_ID` (monotonic, starting at
1; 0 is reserved for "no device"). A grabbed-out device with id 0 can never
match `GRAB_DEVICE`. `unregister_device` removes by id.
- Location: `kernel/src/input/mod.rs` `register_device()`, `unregister_device()`

**INPUT-007 — Grab is exclusive and released by setting 0:**
`grab_device(id)` sets `GRAB_DEVICE`; `read_event` skips events whose
`device_id` differs from the grab. `release_grab()` stores 0. Grab state is a
single relaxed atomic, so it is visible across CPUs and in the ISR-free
consumer path.
- Location: `kernel/src/input/mod.rs` `grab_device()`, `release_grab()`, `read_event()`

**INPUT-008 — Subscribers are notified in consumer context only:**
`read_event` copies each delivered event to every matching subscriber handler
before returning it. Handlers therefore run on the main thread with interrupts
enabled and may use normal kernel services — never from an ISR.
- Location: `kernel/src/input/mod.rs` `read_event()`, `subscribe()`

**INPUT-009 — Poll-driven devices are driven by the consumer:**
When the queue is empty, `read_event` invokes every registered device's `poll`
hook so IRQ-less drivers (e.g. PS/2 polled mode) can submit events. Hooks run
under the `DEVICES` lock, so a hook must not register/unregister devices. If
no hooks exist, `read_event` returns `None` immediately.
- Location: `kernel/src/input/mod.rs` `read_event()`

**INPUT-010 — Events are normalized, not driver-specific:**
`InputEvent { timestamp, device_id, type_, code, value, flags }` follows the
evdev model. `type_` is the device class (`InputType`: Key/Mouse/Touch/
Gamepad/Axis/Custom); `code` is a class-relative code (`KeyCode` for keys);
`value` is press/repeat state (1/2/0) for keys or a signed delta for axes.
`flags` is reserved and always 0 today.
- Location: `kernel/src/input/event.rs`

**INPUT-011 — KeyCodes are physical and layout-independent:**
`KeyCode` is a `#[repr(u32)]`-indexed enum mirroring Linux `KEY_*` values
(`Escape=1` … `Menu=127`). A driver reports *which key*, never *what
character*. `from_code`/`code` provide the only conversions, so unknown codes
surface as `None` rather than mis-translation.
- Location: `kernel/src/input/keycode.rs`

**INPUT-012 — All layout logic lives in the keymap:**
`Keymap::feed` observes every key event to maintain Shift/Ctrl/Alt/Super and
the Caps/Num/Scroll toggles, then yields the character (or `None`) for
consumers. Letter case is `shift XOR caps`; Ctrl+letter yields the classic
control character (0x01..=0x1a); keypad digits resolve via `num XOR shift`
(inverted by Shift per the PS/2 spec). Backspace `\x08`, Tab `\t`, Enter `\n`,
Esc `\x1b`, Delete `\x7f` are returned verbatim so consumers can act on them.
Drivers know nothing of this.
- Location: `kernel/src/input/keymap.rs`

---

## Safety Invariants

**INPUT-S001 — `unsafe impl Sync for InputQueue`:**
Justified by the slot/`seq` round protocol: a slot's `UnsafeCell` is written by
at most one producer per round (CAS-claimed `head`) and read by the consumer
only after observing the published `seq == 2*round + 1` via `Acquire`. Producer
and consumer publish with `Release`, giving a happens-before edge between them;
the slot is re-armed for its next round (`seq = 2*round + 2`) before `tail`
advances. An ISR producer therefore coexists with the main-loop consumer with
no lock and no data race.
- Location: `kernel/src/input/queue.rs`

**INPUT-S002 — No lock is held while dispatching to subscriber handlers:**
`read_event` takes `SUBSCRIBERS` only to copy the handler list, then drops the
lock before invoking handlers, so a handler that calls `subscribe` (or any
input API) cannot deadlock.
- Location: `kernel/src/input/mod.rs` `read_event()`, `subscribe()`

**INPUT-S003 — `read_event` cannot spin indefinitely on poll hooks:**
After invoking poll hooks it re-checks `queue().len()`; if the hooks produced
nothing it returns `None`. A pathological hook that never stops submitting
keeps the consumer busy, but PS/2's hook drains its ring fully per call, so in
practice the loop terminates once the hardware is quiet.
- Location: `kernel/src/input/mod.rs` `read_event()`

---

## API Contracts

**INPUT-API-001 — `pub fn init()`:**
Called from `Kernel::run()` before any driver `register_device`. The queue is
static, so this is a no-op for state setup; it exists for boot-sequence clarity
and future state initialisation. Logs "[uinput] core ready".
- Location: `kernel/src/input/mod.rs` `init()`

**INPUT-API-002 — `pub fn register_device(name: &'static str, capabilities: u32, poll: Option<fn()>) -> u32`:**
Returns the core-owned device id. `capabilities` is a bitmask of `CAP_KEYS` /
`CAP_MOUSE` / `CAP_TOUCH` / `CAP_GAMEPAD` / `CAP_AXIS`. `poll` may be `None`
for pure-IRQ devices; it is the only optional field in the API.
- Location: `kernel/src/input/mod.rs` `register_device()`

**INPUT-API-003 — `pub fn submit_event(ev: InputEvent) -> bool`:**
Safe from interrupt context. Stamps the timestamp and enqueues; `false` means
full (event dropped, counter bumped). Drivers should ignore the result unless
they need drop diagnostics.
- Location: `kernel/src/input/mod.rs` `submit_event()`

**INPUT-API-004 — `pub fn read_event() -> Option<InputEvent>`:**
Non-blocking. Skips grabbed-out devices, dispatches subscriber copies, invokes
poll hooks when empty. Returns `None` when no event is ready.
- Location: `kernel/src/input/mod.rs` `read_event()`

**INPUT-API-005 — `pub fn subscribe(type_: InputType, handler: fn(&InputEvent))`:**
Registers a handler invoked (from `read_event`, consumer context) for every
event of the given class.
- Location: `kernel/src/input/mod.rs` `subscribe()`

**INPUT-API-006 — `pub fn grab_device(id: u32)` / `release_grab()` / `grab_owner()`:**
Grab semantics per INPUT-007.
- Location: `kernel/src/input/mod.rs`

**INPUT-API-007 — `pub fn keymap() -> Keymap`:**
Returns a fresh `Keymap` (all state false) for consumers that want character
translation. `Keymap::feed(&mut self, &InputEvent) -> Option<char>`.
- Location: `kernel/src/input/keymap.rs`

---

## Design Notes

- **Producer/consumer split:** PS/2 registers "PS/2 Keyboard" with
  `CAP_KEYS` and its `poll_device` as the poll hook. Future consumers
  (USB HID, mouse, touch, gamepad drivers and user-space shells) plug in
  with the same API — the queue is MPSC, ids are per-device, and
  subscribers filter by class.
- **Keymap is replaceable:** a Dvorak or national layout is a rewrite of
  `keymap.rs` alone; no driver changes. The keymap observes all events
  (including releases and lock toggles) so its modifier/lock state is
  always in sync without the driver resolving anything.
- **No consumer buffering in drivers:** the old PS/2 `poll_key()`/`KeyEvent`
  API is gone; the driver never holds characters, echoes, or layout state.
- **Why static const queue:** an earlier version heap-allocated the 256-slot
  queue in `input::init()`, which page-faulted at boot — a 10 KB `Box` after
  PCI init could not be satisfied by the fragmented heap. The round-based
  protocol starts every slot at `seq == 0`, so the queue needs no per-slot
  initialisation and is a plain `static const` (no heap, no `Once`, no large
  stack value). This is also why `input::init()` needs no allocation.
