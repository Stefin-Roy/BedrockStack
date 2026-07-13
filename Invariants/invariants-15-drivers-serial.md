# Serial Driver — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/drivers/serial.rs`
**Status:** Stable

---

## State Invariants

**SERIAL-001 — Two-level locking: per-CPU spin-lock then global spin-lock:**
`acquire_locks()` first acquires `pc.serial_locked` (per-CPU), then
`GLOBAL_LOCK` (global). `release_locks()` releases in reverse order.
This prevents deadlock when one CPU holds the global lock and another
spins on it.
- Location: `kernel/src/drivers/serial.rs:104-138`

**SERIAL-002 — `GLOBAL_LOCK` is a spin-lock backed by `AtomicBool`:**
`swap(true, Acquire)` to acquire; `store(false, Release)` to release.
`compiler_fence(SeqCst)` before/after to prevent reordering.
- Location: `kernel/src/drivers/serial.rs:10,112-115,120-123,129,131`

**SERIAL-003 — Before SMP init, only the global lock is taken:**
`try_current_per_cpu()` returns `None` before `early_init_bsp()`.
In that case, `acquire_locks()` avoids the per-CPU lock.
- Location: `kernel/src/drivers/serial.rs:106,118-124`

**SERIAL-004 — `LAST_WAS_NL` tracks line-start state for CPU prefix:**
When `puts()` encounters a `\n`, subsequent output is prefixed with
`[CPU(N)]` at the start of the next line segment.
- Location: `kernel/src/drivers/serial.rs:11,41-59,100-102`

**SERIAL-005 — Raw output functions (`putc`, `put_hex`, `put_u64`)
do NOT add a CPU prefix:**
Only `puts()` manages prefix insertion. The primitives are used as
building blocks inside `puts()` itself.
- Location: `kernel/src/drivers/serial.rs:30,66,73`

---

## Safety Invariants

**SERIAL-S001 — Per-CPU `serial_locked` atomic swap:**
`swap(1, Acquire)` on `pc.serial_locked` — safe because the per-CPU
struct is pinned in static memory and each CPU accesses its own slot
(indexed by `cpu_id`).
- Location: `kernel/src/drivers/serial.rs:107`

**SERIAL-S002 — `compiler_fence` ordering:**
The `SeqCst` fences around lock/unlock prevent the compiler from
reordering memory accesses across the critical section. This is
required because the serial driver uses `AtomicBool` rather than a
full mutex for performance.
- Location: `kernel/src/drivers/serial.rs:110,115,121,129,131,133`

---

## API Contracts

**SERIAL-API-001 — `SerialPort::init()`:**
Initializes the underlying hardware UART (COM1 on x86, MMIO UART on
RISC-V). Called once during `Kernel::new()`.

**SERIAL-API-002 — `SerialPort::puts(s)`:**
Line-buffered output with per-CPU `[CPU(N)]` prefix. Re-entrant safe
(with the two-level lock). Automatically inserts `\r` before `\n`.

**SERIAL-API-003 — `SerialPort::putc(c)` / `put_hex(val)` / `put_u64(val)`:**
Raw output without CPU prefix. Acquires both locks.

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
