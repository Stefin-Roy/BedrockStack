# RISC-V64 Architecture — Invariants

**Version:** 0.4.0
**Date:** 2026-07-31
**Source:** `kernel/src/arch/riscv64/{mod,paging,trap,sbi,trampoline,time}.rs`, `kernel/src/dtb.rs`, `kernel/src/services/riscv64/*`
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

**RISCV-004 — Trap handler saves/restores all 32 GPRs + `sepc` + `sstatus`:**
`__trap_entry` allocates a `TrapFrame` (256 bytes) on the stack,
calls `__trap_handler`, then restores and `sret`.
- Location: `kernel/src/arch/riscv64/trap.rs:23-99,153-186`

**RISCV-005 — SBI ecall interface for firmware operations:**
Console, timer, IPI, HSM (Hart State Management), and SRST (System
Reset) extensions. Uses the standard SBI calling convention:
`a7=extension_id, a6=function_id, a0..a2=args`.
- Location: `kernel/src/arch/riscv64/sbi.rs:23-40`

**RISCV-006 — CPU discovery moved to the `RiscvCpu` service, DTB-first:**
`CpuManager::discover_cpus()` (implemented by `services::riscv64::riscv_cpu::RiscvCpu`)
first parses the device tree via `get_dtb_ptr()` + `dtb::parse_cpus`; if empty
or no DTB, falls back to ACPI MADT (`AcpiSubsystem.cpus`). The BSP's hart ID is
recorded in `Riscv64::init()` from `plic::HART_ID` via `smp::set_bsp_hardware_id()`
before PLIC init.
- Location: `kernel/src/services/riscv64/riscv_cpu.rs:44-57`, `kernel/src/arch/riscv64/mod.rs:20-23`

**RISCV-007 — Identity map covers `[0, max_addr)` without hardcoded 4 GiB ceiling:**
`max_addr = fb_end.max(allocator.alloc_end())`, rounded to 2 MiB.
The hardcoded 4 GiB minimum was removed. MMIO regions (UART at 0x10000000,
PLIC at 0x0C000000, HTIF at 0x40008000) sit below typical RAM (0x80000000)
and are covered automatically.
- Location: `kernel/src/arch/riscv64/paging.rs`

**RISCV-008 — NULL page and stack guard are unmapped:**
Same 4 KiB hole-punching as x86_64 in the identity map loop.
- Location: `kernel/src/arch/riscv64/paging.rs:43-51`

**RISCV-010 — `Riscv64::init()` does NOT wire the UniversalTimer:**
Unlike x86_64, `Riscv64::init()` never calls `universal_timer::early_init`.
The legacy periodic SBI 100 Hz trap timer remains active, and the
`UniversalTimer` service is unwired on riscv64 (would panic if its `Once`
is accessed). `SbiTimer`/`ApicTimer` (`services/*/sbi_timer.rs`,
`apic_timer.rs`) are orphaned.
- Location: `kernel/src/arch/riscv64/mod.rs:16-31`, `kernel/src/services/riscv64/mod.rs`

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
- Location: `kernel/src/arch/riscv64/mod.rs:27-29,37-39,46-58`

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

**RISCV-API-004 — `smp::set_bsp_hardware_id(plic::HART_ID)`:**
Called inside `Riscv64::init()` before `plic::init()` so `scontext()`
can resolve the BSP.

---

## Design Notes

- The RISC-V serial backend uses MMIO at `0x10000000` (QEMU virt
  default), not port I/O. The `IoBackend` trait abstracts this.
- Interrupt sources: PLIC (external), CLINT (timer/software), SBI.
  The `sie` register enables: SEIE (external), SSIE (software), STIE (timer).
- **CLINT is not compiled** — only PLIC and SBI timer are used.
- The `tp` register holds the per-CPU pointer (equivalent to x86 GS.base).
- riscv64's timer story is transitional: the periodic SBI 100 Hz trap timer
  still drives scheduling; the UniversalTimer path is defined but unwired
  (see RISCV-010 and `invariants-23-services.md`).
