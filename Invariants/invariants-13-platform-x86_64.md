# x86_64 Platform Devices — Invariants

**Version:** 0.3.0
**Date:** 2026-07-31
**Source:** `kernel/src/platform/x86_64_pc/{apic,ioapic,pit}.rs`
**Status:** Stable

---

## State Invariants

### Local APIC

**APIC-001 — LAPIC is enabled before timer programming:**
Bit 11 of `IA32_APIC_BASE` MSR is set. Spurious Vector Register
(offset `0xF0`) has bit 8 set. TPR is set to 0.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:406-439`

**APIC-002 — LAPIC timer + TSC calibrated together via PIT:**
PIT channel 0 programmed in one-shot with count `0xFFFF` (~54.9 ms).
APIC timer runs simultaneously with `0xFFFF_FFFF` initial count. From the
elapsed APIC ticks over the PIT interval both frequencies are derived:
- `apic_hz = elapsed * PIT_HZ / PIT_RELOAD` (PIT_HZ = 1_193_182)
- `tsc_hz  = tsc_elapsed * PIT_HZ / PIT_RELOAD`, `TSC_BOOT = tsc_start`
- The per-interrupt count for `TIMER_HZ = 1000` is `count = apic_hz / TIMER_HZ`.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:285-376`
- Note: `rdtsc` is used to bracket the PIT interval so TSC is calibrated in the same pass.

**APIC-003 — APIC timer is a masked one-shot, armed only on demand:**
The LVT Timer register (offset `0x320`) is configured with the mask bit
(bit 16) SET and vector 32. The UniversalTimer's clockevent arms a single
shot via `oneshot_timer_set(count)` (LVT = vector, no mask) and cancels via
`timer_stop()` (LVT |= mask, init count = 0). There is **no periodic mode**.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:171-185,406-451`
- `TIMER_VECTOR: u8 = 32` — `apic.rs:22`

**APIC-004 — x2APIC mode is forced OFF:**
QEMU TCG sets the CPUID x2APIC bit and MSR `0x1B` bit 10, but the x2APIC
MSR range causes #GP regardless. Bit 10 is cleared and `X2APIC_MODE` stays
`false`; all access goes through MMIO (`LAPIC_BASE + reg`).
- Location: `kernel/src/platform/x86_64_pc/apic.rs:426-433`

**APIC-005 — APs arm their own timer on demand; count shared:**
The calibrated count is stored in `BSP_TIMER_COUNT` (global `AtomicU32`).
`init_ap()` skips PIT calibration, does **not** write `IA32_APIC_BASE` (may
#GP on many CPUs), and programs its LVT timer as masked one-shot with init
count 0. Once the per-CPU universal timer arms it (a timer pinned to that
AP's base), the AP's own LAPIC fires and its own ISR processes the base.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:401-429`

**APIC-006 — PIT calibration has a 1,000,000-ticks fallback:**
If PIT times out, yields zero elapsed ticks, or the derived count is 0, the
calibration returns `1_000_000` (works on QEMU at 100 MHz APIC frequency).
A TSC-elapsed-zero warning falls back to the APIC-based clocksource.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:305-373`

**APIC-007 — IPI delivery waits for previous IPI to complete (xAPIC):**
In xAPIC mode, the delivery status bit (bit 12 of ICR low) must be 0
before a new IPI is sent. Broadcast-to-all-except-self uses ICR
destination shorthand (bits 18:16 = 11).
- Location: `kernel/src/platform/x86_64_pc/apic.rs:37-51,238-254`

**APIC-008 — IPI vectors are fixed: 49 (resched), 50 (TLB shootdown), 51 (halt), 52 (timer reschedule):**
Vector 52 (`IPI_TIMER`) is the cross-CPU timer-reschedule hint: `UniversalTimer`
sends it to a target CPU when a remote set/migrate moves that CPU's earliest
deadline earlier; the target re-runs `tick()` on its own base.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:280-292`

**APIC-009 — `PollTimeout` is TSC-backed, replacing the old APIC-counter `ApicTimeout`:**
Works after `apic::init()` calibration completes, with no dependency on a
running periodic APIC timer. Deadline computed in ns from `tsc_now_ns()`.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:114-130`

### I/O APIC

**IOAPIC-001 — All redirection entries masked after init:**
No stray interrupts fire before entries are explicitly configured.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:80-83`

**IOAPIC-002 — I/O APIC registers accessed via volatile MMIO:**
MMIO region mapped as RW + NO_CACHE. Read/write sequences use the
Intel-specified index/data register pair.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:32-51`

**IOAPIC-003 — Redirection entry writes: high DWORD first, then low:**
Per Intel specification, the low DWORD write triggers the update.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:121-124`

**IOAPIC-004 — Global state behind `Mutex<Option<IoApicState>>`:**
All operations lock the global mutex. `enable_irq` returns `None` if
GSI not managed by this IOAPIC or if vectors exhausted.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:30,96-98,101-107`

### PIT

**PIT-001 — PIT is programmed in one-shot mode (command 0x30):**
Count written low-byte then high-byte to data port 0x40.
- Location: `kernel/src/platform/x86_64_pc/pit.rs:16-20`

**PIT-002 — `has_fired()` reads back status via command 0xE2:**
Checks bit 7 of the returned status (output pin status = 1 when
the count reaches zero and the output goes high).
- Location: `kernel/src/platform/x86_64_pc/pit.rs:22-25`

---

## Safety Invariants

**APIC-S001 — MSR read/write safety:**
`rdmsr`/`wrmsr` use inline asm. Valid MSR indices must be provided.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:53-63`

**APIC-S002 — LAPIC MMIO access safety (xAPIC mode):**
`LAPIC_BASE` is read from `IA32_APIC_BASE` MSR. The computed register
address must be within the LAPIC MMIO frame and must be mapped in the
page tables.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:81-97`

**APIC-S003 — `init_ap` must not write `IA32_APIC_BASE`:**
Writes from a non-BSP processor may cause a general-protection exception
(Intel SDM). APs only read the base; mode is forced xAPIC globally by the BSP.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:387-391`

**IOAPIC-S001 — I/O APIC MMIO access safety:**
`ioapic_write`/`ioapic_read` use volatile pointer operations on the
mapped virtual address. The address is validated at init time.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:32-46`

**PIT-S001 — Port I/O safety:**
`outb`/`inb` use inline asm. PIT ports `0x40`/`0x43` are standard
ISA ports and safe to access on any x86 PC.
- Location: `kernel/src/platform/x86_64_pc/pit.rs:5-14`

---

## API Contracts

**APIC-API-001 — `apic::init()`:**
Enables LAPIC, forces xAPIC mode, calibrates via PIT, stores
`BSP_TIMER_COUNT`/`TSC_HZ`/`TSC_BOOT`/`APIC_HZ`, leaves the timer masked
(one-shot, never fired). Panics if CPU has no local APIC.

**APIC-API-002 — `apic::init_ap()`:**
AP-only init. Enables LAPIC, sets SVR/TPR, leaves the timer masked with
init count 0. Does NOT calibrate PIT and does NOT write `IA32_APIC_BASE`.

**APIC-API-003 — `apic::apic_eoi()`:**
Writes 0 to the EOI register. Called by the timer and device IRQ handlers.

**APIC-API-004 — `apic::oneshot_timer_set(count)` / `apic::timer_stop()`:**
Arms/cancels a single one-shot APIC timer interrupt on vector 32. These
are the clockevent's only entry points.

**APIC-API-005 — `apic::send_ipi(dest_apic_id, vector)` and friends:**
`send_init_ipi`, `send_init_deassert`, `send_sipi_ipi(page)` implement the
INIT-INIT-SIPI MP startup sequence; `send_ipi_all_except_self` broadcasts.

**APIC-API-006 — `apic::tsc_hz()`, `tsc_boot()`, `apic_hz()`, `tsc_now_ns()`, `timer_init_count()`:**
Read accessors for calibration results. `tsc_now_ns()` uses divide-first
arithmetic to avoid u64 overflow and returns 0 before calibration.

**APIC-API-007 — `PollTimeout::new(ms)` / `.expired()`:**
TSC-backed deadline for driver polling loops (AHCI, xHCI). Replaces the
removed APIC-counter-based `ApicTimeout`.

**IOAPIC-API-001 — `ioapic::init(phys_base, gsi_base)`:**
Initializes I/O APIC from ACPI MADT data. Masks all entries.

**IOAPIC-API-002 — `ioapic::enable_irq(gsi, polarity, trigger) → Option<u8>`:**
Assigns a vector (≥33) to the specified GSI. Returns `None` if the GSI
is not managed by this IOAPIC or if vectors exhausted.

**IOAPIC-API-003 — `ioapic::mask_irq(gsi)` / `ioapic::unmask_irq(gsi)`:**
Masks/unmasks the redirection entry for a GSI.

**PIT-API-001 — `pit::program_one_shot(count)`:**
Starts a one-shot countdown on PIT channel 0.

**PIT-API-002 — `pit::has_fired() → bool`:**
Returns true when the one-shot countdown has completed.

---

## Design Notes

- The LAPIC timer is now **exclusively** the UniversalTimer's clockevent
  backend (one-shot deadlines), not a periodic tick source. `TIMER_HZ = 1000`
  is retained only as the calibration target for the count value. See
  `invariants-23-services.md`.
- Each CPU's LAPIC timer is armed on demand by that CPU's own timer base;
  `apic::oneshot_timer_set`/`timer_stop` always program the *current* CPU's
  LAPIC (shared MMIO VA), so the clockevent only ever runs while on the CPU
  whose base it serves.
- `PollTimeout` (TSC-backed) is the driver polling primitive; the old
  APIC-counter `ApicTimeout` was removed in commit `8c515c4`.
