# x86_64 Platform Devices — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/platform/x86_64_pc/{apic,ioapic,pit}.rs`
**Status:** Stable

---

## State Invariants

### Local APIC

**APIC-001 — LAPIC is enabled before timer programming:**
Bit 11 of `IA32_APIC_BASE` MSR is set. Spurious Vector Register
(offset `0xF0`) has bit 8 set. TPR is set to 0.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:333-354`

**APIC-002 — LAPIC timer calibrated via PIT before starting:**
PIT channel 0 programmed in one-shot with count `0xFFFF` (~54.9 ms).
APIC timer runs simultaneously with `0xFFFF_FFFF` initial count.
Elapsed APIC ticks during the PIT interval compute the count for
`TIMER_HZ = 1000` (1 ms period).
- Location: `kernel/src/platform/x86_64_pc/apic.rs:209-279`
- Formula: `count = elapsed_ticks * 1193182 / (6553500 * TIMER_HZ factor)`

**APIC-003 — APIC timer interrupt at vector 32:**
LVT Timer register (offset `0x320`) configured with periodic mode
(bit 17) and vector 32. Timer handler writes EOI (offset `0xB0`).
- Location: `kernel/src/platform/x86_64_pc/apic.rs:23,359-362`

**APIC-004 — x2APIC mode enabled when supported:**
If CPUID indicates x2APIC support, the enable bit (bit 10) is set in
`IA32_APIC_BASE`. All register access uses `rdmsr`/`wrmsr` in x2APIC
mode instead of MMIO.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:340-345,87-103`

**APIC-005 — BSP timer count shared with APs:**
The calibrated count is stored in `BSP_TIMER_COUNT` (global `AtomicU32`).
APs read it during `init_ap()` to program their local timers.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:207,310-313,357`

**APIC-006 — PIT calibration has fallback:**
If PIT times out or yields zero elapsed ticks, a hard-coded fallback
of 1,000,000 ticks is used (works on QEMU at 100 MHz APIC frequency).
- Location: `kernel/src/platform/x86_64_pc/apic.rs:235-236,248-249,274-276`

**APIC-007 — IPI delivery waits for previous IPI to complete (xAPIC):**
In xAPIC mode, the delivery status bit (bit 12 of ICR low) must be 0
before a new IPI is sent.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:57-58`

### I/O APIC

**IOAPIC-001 — All redirection entries masked after init:**
No stray interrupts fire before entries are explicitly configured.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:80-83`

**IOAPIC-002 — I/O APIC registers accessed via volatile MMIO:`
MMIO region mapped as RW + NO_CACHE. Read/write sequences use the
Intel-specified index/data register pair.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:32-51`

**IOAPIC-003 — Redirection entry writes: high DWORD first, then low:`
Per Intel specification, the low DWORD write triggers the update.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:121-124`

**IOAPIC-004 — Global state behind `Mutex<Option<IoApicState>>`:`
All operations lock the global mutex. `enable_irq` returns `None` if
GSI not managed by this IOAPIC or if vectors exhausted.
- Location: `kernel/src/platform/x86_64_pc/ioapic.rs:30,96-98,101-107`

### PIT

**PIT-001 — PIT is programmed in one-shot mode (command 0x30):**
Count written low-byte then high-byte to data port 0x40.
- Location: `kernel/src/platform/x86_64_pc/pit.rs:16-20`

**PIT-002 — `has_fired()` reads back status via command 0xE2:`
Checks bit 7 of the returned status (output pin status = 1 when
the count reaches zero and the output goes high).
- Location: `kernel/src/platform/x86_64_pc/pit.rs:22-25`

---

## Safety Invariants

**APIC-S001 — MSR read/write safety:**
`rdmsr`/`wrmsr` use inline asm. Valid MSR indices must be provided.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:66-76`

**APIC-S002 — LAPIC MMIO access safety (xAPIC mode):**
`LAPIC_BASE` is read from `IA32_APIC_BASE` MSR. The computed register
address must be within the LAPIC MMIO frame and must be mapped in the
page tables.
- Location: `kernel/src/platform/x86_64_pc/apic.rs:91-92,100-101`

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
Enables LAPIC, calibrates timer, starts periodic timer at `TIMER_HZ`.
Panics if CPU has no local APIC.

**APIC-API-002 — `apic::init_ap()`:**
AP-only init. Enables LAPIC and starts timer using BSP's calibrated
count. Does NOT calibrate PIT.

**APIC-API-003 — `apic::apic_eoi()`:**
Writes 0 to the EOI register. Called by the timer and device IRQ
handlers.

**APIC-API-004 — `apic::send_ipi(dest_apic_id, vector)`:**
Sends fixed IPI. Used for TLB shootdown and reschedule IPIs.

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
