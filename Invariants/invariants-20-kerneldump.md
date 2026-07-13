# Kernel Fault Dump — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/kerneldump/{mod,dump,disasm}.rs`
**Status:** Stable (x86_64 only)

---

## State Invariants

**DUMP-001 — Fault dump is guarded against re-entrancy:**
`DUMP_IN_PROGRESS` (AtomicBool) prevents a fault within the dump code
from recursively invoking the dump. If already dumping, the handler
halts immediately.
- Location: `kernel/src/kerneldump/mod.rs`

**DUMP-002 — Dump output includes all critical register state:**
On x86_64: all GPRs, CR0/CR2/CR3/CR4, RFLAGS, RSP, stack trace,
page fault info (CR2, error code), and disassembly around RIP.
- Location: `kernel/src/kerneldump/dump.rs`

**DUMP-003 — Disassembler covers x86_64 common instructions:**
`kernel/src/kerneldump/disasm.rs` implements a simple decoder capable
of printing the bytes around the faulting RIP.

---

## Safety Invariants

**DUMP-S001 — Volatile register reads are safe:**
CR registers and other MSRs are read via inline asm. These reads do
not have side effects and are safe to execute at any point.
- Location: `kernel/src/kerneldump/dump.rs`

**DUMP-S002 — Stack trace reads must not fault:**
The stack pointer is read from the exception frame. Walking the stack
could encounter an unmapped or invalid address on a corrupted stack,
but this is acceptable because the dump is best-effort.

---

## API Contracts

**DUMP-API-001 — `dump_full_fault(vector, error_code, frame)`:**
Called by all exception handlers (IDT entries for faults). Prints
register state, stack trace, and disassembly to serial. Does not return.

---

## Design Notes

- Kerneldump is x86_64 only (behind `#[cfg(target_arch = "x86_64")]`).
- All exception handlers delegate to `dump_full_fault()` except
  breakpoint (which is intentionally handled differently).
- The double-fault handler uses an IST stack, so even if the kernel
  stack overflowed, the dump code runs on a known-good stack.
