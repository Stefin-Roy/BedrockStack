# RISC-V Platform Devices — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/platform/riscv_virt/{mod,plic,clint,htif}.rs`
**Status:** Stable

---

## State Invariants

### PLIC (Platform-Level Interrupt Controller)

**PLIC-001 — PLIC is initialized once during `Arch::init()`:**
The QEMU virt machine PLIC base address (`0xC000000`) is mapped and
priority/threshold registers are configured.
- Location: `kernel/src/platform/riscv_virt/plic.rs`

**PLIC-002 — Hart ID is stored in a global `AtomicUsize`:`
Used for SBI communication and per-CPU identification.
- Location: `kernel/src/platform/riscv_virt/plic.rs`

### CLINT (Core Local Interrupt Controller)

**CLINT-003 — CLINT is mapped for timer and software interrupts:**
QEMU virt machine CLINT base at `0x2000000`. Currently a stub —
timer interrupts are managed via SBI `set_timer` ecalls.
- Location: `kernel/src/platform/riscv_virt/clint.rs`

### HTIF (Host-Target Interface)

**HTIF-004 — HTIF provides console putchar/getchar on QEMU:`
Alternative to SBI legacy console. Base at `0x40008000` (QEMU virt).
- Location: `kernel/src/platform/riscv_virt/htif.rs`

---

## Safety Invariants

**PLIC-S001 — PLIC MMIO access safety:**
The PLIC base address is mapped as RW + NO_CACHE via the VMM before
register access.
- Location: `kernel/src/platform/riscv_virt/plic.rs`

**CLINT-S002 — CLINT MMIO access safety:**
Same mapping discipline as PLIC.

---

## API Contracts

**PLIC-API-001 — `plic::init()`:**
Maps PLIC MMIO, configures priority thresholds. Called during
`Arch::init()`.

**PLIC-API-002 — `plic::enable_irq(irq_id, hart_id)`:**
Enables a specific interrupt for a given hart.

**CLINT-API-001 — `clint::set_timer(stime_value)`:`
Programs the `mtimecmp` register (or delegates to SBI).

**HTIF-API-001 — `htif::putchar(c)` / `htif::getchar() → i32`:`
Blocking console I/O.

---

## Design Notes

- QEMU virt machine has a fixed memory layout: PLIC at `0xC000000`,
  CLINT at `0x2000000`, UART at `0x10000000`.
- The RISC-V platform code is less mature than x86_64; many components
  (CLINT timer, HTIF) are stubs that delegate to SBI.
- `sbi::set_timer()` is the primary timer interface, using the SBI
  legacy extension.
