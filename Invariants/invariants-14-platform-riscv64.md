# RISC-V Platform Devices — Invariants

**Version:** 0.3.0
**Date:** 2026-07-31
**Source:** `kernel/src/platform/riscv_virt/{mod,plic,clint,htif}.rs`, `kernel/src/services/riscv64/riscv_interrupts.rs`
**Status:** Stable

---

## State Invariants

### PLIC (Platform-Level Interrupt Controller)

**PLIC-001 — PLIC is initialized once during `Riscv64::init()`:**
The QEMU virt machine PLIC base address (`0x0C000000`) is accessed via
raw MMIO pointers. `init()` sets all 127 source priorities to 0
(disabled), clears all enable bits for the S-mode context, and sets the
context threshold to 0 (accept all).
- Location: `kernel/src/platform/riscv_virt/plic.rs:40-55`, caller `kernel/src/arch/riscv64/mod.rs:25`

**PLIC-002 — Hart ID is stored in a global `AtomicUsize`:**
`plic::HART_ID` defaults to `usize::MAX` and is set from the boot entry
point (`main.rs:124`). `Riscv64::init()` reads it for the S-mode context.
The S-mode PLIC context is computed as `hart_id * 2 + 1` (QEMU virt
provides 2 contexts per hart: M-mode and S-mode).
- Location: `kernel/src/platform/riscv_virt/plic.rs:100-109`, `kernel/src/main.rs:124`, `kernel/src/arch/riscv64/mod.rs:22`

**PLIC-003 — DTB pointer is stashed for CPU discovery:**
`set_dtb_ptr(ptr)` (called from the riscv64 boot entry) stores the device
tree pointer in `DTB_PTR: AtomicUsize`. `get_dtb_ptr()` returns it, used by
`RiscvCpu::discover_cpus()` as the primary source, with ACPI as fallback.
- Location: `kernel/src/platform/riscv_virt/mod.rs:9-17`, `kernel/src/main.rs:121`, `kernel/src/services/riscv64/riscv_cpu.rs:45`

### CLINT (Core Local Interrupt Controller)

**CLINT-004 — CLINT is NOT compiled:**
`clint.rs` contains only a comment: CLINT registers are at `0x02000000`
but PMP-protected by OpenSBI and NOT accessible from S-mode. The module
is **not declared** in `riscv_virt/mod.rs` (only `plic` and `htif` are).
Timer interrupts are managed via SBI `set_timer` ecalls instead.
- Location: `kernel/src/platform/riscv_virt/clint.rs:1-2`, `kernel/src/platform/riscv_virt/mod.rs:1`

### HTIF (Host-Target Interface)

**HTIF-005 — HTIF provides machine shutdown on QEMU:**
`htif::shutdown()` writes the power-off command to `tohost` at
`0x40008000`, waits for `fromhost` acknowledgement, then halts. No
console putchar/getchar exists.
- Location: `kernel/src/platform/riscv_virt/htif.rs`

### InterruptManager service

**PLIC-006 — `RiscvInterrupts` provides the `InterruptManager` capability:**
A 127-entry `PLIC_HANDLERS` table of `AtomicPtr<fn()>` maps PLIC source
numbers to handlers. `dispatch_external()` claims the pending PLIC IRQ,
runs the handler, and completes (EOI). `register_handler`/`unregister_handler`/
`enable`/`disable` are bounds-checked; `eoi` is a no-op. `enable()` forwards
to `plic::enable_irq`.
- Location: `kernel/src/services/riscv64/riscv_interrupts.rs:8-23,33-56`

---

## Safety Invariants

**PLIC-S001 — PLIC MMIO access safety:**
The PLIC base address is accessed via raw volatile pointers. These
addresses are within the identity-mapped/device range; the PLIC is
mapped RW + NO_CACHE via the VMM at page-table setup.
- Location: `kernel/src/platform/riscv_virt/plic.rs:7-38,40-98`

---

## API Contracts

**PLIC-API-001 — `plic::init()`:**
Disables all sources (priority 0), clears S-mode enable bits, sets
threshold 0. Called once during `Riscv64::init()`.

**PLIC-API-002 — `plic::enable_irq(irq: u32)` / `disable_irq(irq: u32)`:**
Enables/disables a single interrupt source for the current S-mode context
(sets/clears the bit in the enable word). Single-argument form — the hart
is derived from `smp::current_per_cpu().apic_id`, not passed in.

**PLIC-API-003 — `plic::set_priority(irq, priority)`, `claim()`, `complete(irq)`:**
`set_priority` writes `priority & 7`. `claim()` returns the highest-priority
pending IRQ (0 if none). `complete(irq)` writes back to the claim register
as EOI.

**HTIF-API-001 — `htif::shutdown() -> !`:**
Writes the power-off command to `tohost` and waits for acknowledgement,
then halts forever. Does not return.

---

## Design Notes

- QEMU virt machine has a fixed memory layout: PLIC at `0x0C000000`,
  UART at `0x10000000`. CLINT at `0x02000000` is PMP-protected by OpenSBI
  and unusable from S-mode.
- Timer and shutdown primitives go through SBI ecalls
  (`sbi::set_timer`, `sbi::system_reset`), not direct MMIO.
- `plic::scontext()` reads the current hart from
  `smp::current_per_cpu().apic_id`, so per-hart PLIC state requires the
  per-CPU area to be valid.
