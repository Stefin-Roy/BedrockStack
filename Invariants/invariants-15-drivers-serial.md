# Serial Driver — Invariants

**Version:** 0.3.0
**Source:** `kernel/src/drivers/serial.rs`
**Status:** Stable

---

## State Invariants

**SERIAL-001 — Two-level locking: per-CPU spin-lock then global spin-lock:**
`acquire_locks()` first acquires `pc.serial_locked` (per-CPU, using
`AtomicU64::swap`), then `GLOBAL_LOCK` (global `AtomicBool`). `release_locks()`
releases in reverse order. This prevents deadlock when one CPU holds the
global lock and another spins on it.
- Location: `kernel/src/drivers/serial.rs:200-238`

**SERIAL-002 — `GLOBAL_LOCK` is a spin-lock backed by `AtomicBool`:**
`swap(true, Acquire)` to acquire; `store(false, Release)` to release.
`compiler_fence(SeqCst)` before/after to prevent reordering.
- Location: `kernel/src/drivers/serial.rs:24,209-213,218-222,229-231`

**SERIAL-003 — Before SMP init, only the global lock is taken:**
`try_current_per_cpu()` returns `None` before `early_init_bsp()`.
In that case, `acquire_locks()` avoids the per-CPU lock and returns `None`.
- Location: `kernel/src/drivers/serial.rs:200-225`

**SERIAL-004 — `LAST_WAS_NL` tracks line-start state for CPU prefix:**
When `puts()` encounters a `\n`, subsequent output is prefixed with
`[CPU(N)]` at the start of the next line segment. The prefix only appears
when a `PerCpu` struct is available (i.e. after `early_init_bsp()`).
- Location: `kernel/src/drivers/serial.rs:25,56-87,165-167`

**SERIAL-005 — Raw output functions (`putc`, `put_hex`, `put_u64`) do NOT
add a CPU prefix, but DO acquire locks and mirror to framebuffer:**
Only `puts()` manages prefix insertion. The primitives are used as building
blocks inside `puts()` itself. When `feature = "display_log"` is enabled,
they also write to the framebuffer `Console` via `putc_and_flush()`/`puts()`.
- Location: `kernel/src/drivers/serial.rs:44-53,90-100,103-113`

**SERIAL-006 — Lock-free `dump_*` functions bypass all locks:**
`dump_putc()`, `dump_puts()`, `dump_put_hex()`, `dump_put_u64()` write
directly to the inner `SerialPort` without acquiring the per-CPU or global
lock. Safe ONLY during a fault dump (single CPU, interrupts disabled, no
concurrent access). Used by `kerneldump` to report crash state.
- Location: `kernel/src/drivers/serial.rs:123-147`

**SERIAL-007 — Framebuffer console is set via `set_console()`:**
Behind `#[cfg(feature = "display_log")]`, a global `Console` instance is
stored in a `ConsoleCell(UnsafeCell<Option<Console>>)`. `set_console()` is
called during `Kernel::init()` after page tables are live. All `putc`,
`puts`, `put_hex`, `put_u64` mirror output to the console while holding
locks, ensuring serial and display output are synchronized.
- Location: `kernel/src/drivers/serial.rs:13-22,149-152`

**SERIAL-008 — `format_hex`/`format_dec` helpers for display mirroring:**
Behind `#[cfg(feature = "display_log")]`, `put_hex`/`put_u64` use
`format_hex()`/`format_dec()` to produce string slices for the console,
avoiding allocation in the locked context.
- Location: `kernel/src/drivers/serial.rs:170-198`

**SERIAL-009 — `write_prefix()` outputs `[CPU(N)] ` directly to Inner:**
Called from `puts()` when a new line starts. Writes the raw prefix via
the inner serial port (no recursive locking) and does not affect
`LAST_WAS_NL` tracking.
- Location: `kernel/src/drivers/serial.rs:154-163`

---

## Safety Invariants

**SERIAL-S001 — Per-CPU `serial_locked` atomic swap:**
`swap(1, Acquire)` on `pc.serial_locked` — safe because the per-CPU
struct is pinned in static memory and each CPU accesses its own slot
(indexed by `cpu_id`).
- Location: `kernel/src/drivers/serial.rs:202-206`

**SERIAL-S002 — `compiler_fence` ordering:**
The `SeqCst` fences around lock/unlock prevent the compiler from
reordering memory accesses across the critical section. This is
required because the serial driver uses `AtomicBool` rather than a
full mutex for performance.
- Location: `kernel/src/drivers/serial.rs:207,214,221,229,231,233`

**SERIAL-S003 — `dump_*` safety:**
Must only be called when the calling CPU holds both locks implicitly
(interrupts disabled, no other code running). Bypassing the lock
mechanism during a fault dump is safe because the fault handler runs
on a single CPU with interrupts masked.

**SERIAL-S004 — `ConsoleCell` / `set_console` safety:**
`ConsoleCell` wraps `UnsafeCell<Option<Console>>` and is `Sync`.
It is written exactly once during `Kernel::init()` (from the BSP, before
SMP starts) and only read under the serial lock thereafter. The `UnsafeCell`
is justified because all access is serialized by the serial locks.

---

## API Contracts

**SERIAL-API-001 — `SerialPort::init()`:**
Initializes the underlying hardware UART (COM1 on x86, MMIO UART on
RISC-V). Called once during `Kernel::new()`.

**SERIAL-API-002 — `SerialPort::puts(s)`:**
Line-buffered output with per-CPU `[CPU(N)]` prefix. Re-entrant safe
(with the two-level lock). When `feature = "display_log"` is enabled,
mirrors output to the framebuffer console after writing to serial.

**SERIAL-API-003 — `SerialPort::putc(c)` / `put_hex(val)` / `put_u64(val)`:**
Raw output without CPU prefix. Acquires both locks. Mirrors to framebuffer
console when `feature = "display_log"` is enabled.

**SERIAL-API-004 — `dump_putc(c)` / `dump_puts(s)` / `dump_put_hex(val)` / `dump_put_u64(val)`:**
Lock-free output that bypasses all spinlocks. Only safe during a fault dump
(single CPU, interrupts disabled).

**SERIAL-API-005 — `set_console(console)`:**
Registers a framebuffer `Console` for display mirroring. Called once during
`Kernel::init()` after page tables cover the framebuffer.

---

## Design Notes

- The per-CPU lock prevents re-entrancy on the same CPU (e.g., if an
  interrupt handler calls `puts()` while the main thread holds the
  serial lock, it spins on its own per-CPU lock, which is already held
  → would deadlock without two-level design; the per-CPU lock prevents
  the BSP from grabbing the global lock twice).
- The inner `common::serial::SerialPort` has a timeout mechanism:
  if the transmitter stays busy for ~100K iterations, data is written
  anyway (best-effort) to avoid hanging the kernel.
- `core::fmt::Write` is implemented for `SerialPort` via `write_str`.
- The `display_log` feature gates all framebuffer console mirroring.
  `ConsoleCell` uses `UnsafeCell` to avoid the overhead of `Mutex` on
  every character output; the serial locks already serialize access.
- `format_hex` and `format_dec` are stack-allocated formatters that
  produce `&str` slices for console output without heap allocation.
