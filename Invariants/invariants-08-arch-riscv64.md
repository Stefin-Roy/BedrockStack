# RISC-V64 Architecture — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/arch/riscv64/{mod,paging,trap,sbi,trampoline,serial}.rs`, `kernel/src/dtb.rs`
**Status:** Stable

---

## State Invariants

**RISCV-001 — Sv39 paging with hand-rolled page tables:**
No external crate dependency. 4 KiB and 2 MiB pages supported.
Identity mapping + higher-half kernel alias at `KERNEL_VMA_BASE + phys`.
- Location: `kernel/src/arch/riscv64/paging.rs`

**RISCV-002 — W^X enforced (identical logic to x86_64):**
`.text` = READ + EXECUTE, `.rodata` = READ, everything else = READ + WRITE
with NX. Framebuffer area strips EXECUTE.
- Location: `kernel/src/arch/riscv64/paging.rs:80-90`

**RISCV-003 — NULL page and stack guard are unmapped:**
Same 4 KiB hole-punching as x86_64 in the identity map loop.
- Location: `kernel/src/arch/riscv64/paging.rs:43-51`

**RISCV-004 — Trap handler saves/restores all 32 GPRs + `sepc` + `sstatus`:**
`__trap_entry` allocates a `TrapFrame` (256 bytes) on the stack,
calls `__trap_handler`, then restores and `sret`.
- Location: `kernel/src/arch/riscv64/trap.rs:23-99,153-186`

**RISCV-005 — SBI ecall interface for firmware operations:**
Console, timer, IPI, HSM (Hart State Management), and SRST (System
Reset) extensions. Uses the standard SBI calling convention:
`a7=extension_id, a6=function_id, a0..a2=args`.
- Location: `kernel/src/arch/riscv64/sbi.rs:23-40`

**RISCV-006 — CPU discovery via DTB or ACPI MADT:**
First tries DTB parsing (`crate::dtb::parse_cpus`), falls back to
ACPI MADT data. BSP hart ID read from PLIC.
- Location: `kernel/src/arch/riscv64/mod.rs:74-89`

---

## Safety Invariants

**RISCV-S001 — `trap::init()` safety:**
Writes `stvec` CSR with the address of `__trap_entry`. Must be called
before any interrupts are enabled.
- Location: `kernel/src/arch/riscv64/trap.rs:147-151`

**RISCV-S002 — SBI `ecall` safety:**
The inline asm `ecall` uses `options(nomem, nostack)` because SBI
calls don't access the caller's memory or stack.
- Location: `kernel/src/arch/riscv64/sbi.rs:23-40`

**RISCV-S003 — CSR manipulation safety:**
`sstatus`, `sie`, `stvec` are written via inline asm. The caller
must understand the RISC-V privilege specification.
- Location: `kernel/src/arch/riscv64/mod.rs:30-32,49-61`

---

## API Contracts

**RISCV-API-001 — `sbi::hart_start(hart_id, start_addr, priv)`:**
Starts an AP at `start_addr` in supervisor mode. Returns `true`
on success. Used by `trampoline::start_aps()`.
- Location: `kernel/src/arch/riscv64/sbi.rs:79-87`

**RISCV-API-002 — `sbi::system_reset()` / `sbi::cold_reboot()`:**
SRST extension. Does not return on success. Falls back to infinite
`wfi` loop on failure.
- Location: `kernel/src/arch/riscv64/sbi.rs:69-78`

**RISCV-API-003 — `sbi::set_timer(stime_value)`:**
Programs the next timer interrupt. The `stime_value` is an absolute
time in the `mtime` CSR's timebase.
- Location: `kernel/src/arch/riscv64/sbi.rs:52-56`

---

## Design Notes

- The RISC-V serial backend uses MMIO at `0x10000000` (QEMU virt
  default), not port I/O. The `IoBackend` trait abstracts this.
- Interrupt sources: PLIC (external), CLINT (timer/software), SBI.
  The `sie` register enables: SEIE (external), SSIE (software), STIE (timer).
- The `tp` register holds the per-CPU pointer (equivalent to x86 GS.base).
