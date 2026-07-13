# SMP — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/smp/mod.rs`
**Status:** Stable

---

## State Invariants

**SMP-001 — `PerCpu` struct is `#[repr(C)]` with `self_ptr` as first field:**
The first field MUST be `self_ptr: *const PerCpu` pointing to itself.
On x86_64, it is accessed via `gs:[0]` (GS.base = &PerCpu). On RISC-V,
via the `tp` register. This allows `current_per_cpu()` to read the
pointer from a known offset.
- Location: `kernel/src/smp/mod.rs:7-20,47-62`

**SMP-002 — Maximum 16 CPUs:**
`MAX_CPUS = 16`. `PER_CPU_SLOTS` is a fixed-size array of 16 entries.
`per_cpu_by_id()` asserts `cpu_id < MAX_CPUS`.
- Location: `kernel/src/smp/mod.rs:23-24,74-77`

**SMP-003 — BSP is CPU 0, initialized at boot:**
`early_init_bsp()` sets `cpu_id = 0, is_bsp = true` and stores 1 in
`started`. APs start with `started = 0` and wait for the BSP to set
it to 1 (busy-wait in trampoline epilogue).
- Location: `kernel/src/smp/mod.rs:91-104`

**SMP-004 — AP stacks are 17 pages (64 KB + 4 KB guard) from `alloc_contiguous`:**
`AP_STACK_PAGES = 17`. The returned `stack_top` is the highest address
in the allocated range. The lowest page the guard (unmapped by paging).
- Location: `kernel/src/smp/mod.rs:194-199`

**SMP-005 — `SMP init` runs after ACPI, page tables, and I/O APIC:**
`Kernel::init()` calls `smp::init()` after `init_acpi()` and
`init_ioapic()`, but before `enable_interrupts()`.
- Location: `kernel/src/lib.rs:162-165`

**SMP-006 — CPU_COUNT reflects total online CPUs:**
Stored as `AtomicU32`, set during `smp::init()` before APs are started.
- Location: `kernel/src/smp/mod.rs:25,80-81,174`

**SMP-007 — AP `started` flag is an `AtomicU64`:**
The trampoline code polls this address until it becomes non-zero.
The BSP writes 1 after the AP completes its initialization.
- Location: `kernel/src/smp/mod.rs:18,159-160`

---

## Safety Invariants

**SMP-S001 — `early_init_bsp()` safety:**
Must be called exactly once on the BSP before any SMP operations.
Writes to `PER_CPU_SLOTS[0]` (static mut) and sets GS.base / tp.
- Location: `kernel/src/smp/mod.rs:91-104`

**SMP-S002 — `smp::init()` safety:**
Must be called after heap, page tables, ACPI, and I/O APIC init.
Allocates AP stacks from the physical allocator. Executes arch-specific
`wake_aps()` which issues IPIs/SBI calls.
- Location: `kernel/src/smp/mod.rs:136-192`

**SMP-S003 — `current_per_cpu()` safety (x86_64):**
Reads `gs:[0]` via inline asm. GS.base must have been set by
`early_init_bsp()` (or by the AP trampoline), and `self_ptr` must
point to the correct `PerCpu` slot.
- Location: `kernel/src/smp/mod.rs:47-53`

**SMP-S004 — `current_per_cpu()` safety (RISC-V):**
Reads the `tp` register. Must have been set by `early_init_bsp()` or
AP trampoline.
- Location: `kernel/src/smp/mod.rs:56-62`

---

## API Contracts

**SMP-API-001 — `smp::early_init_bsp()`:**
Initializes PerCpu slot for BSP. Called early in `Kernel::init()`.

**SMP-API-002 — `smp::init(alloc, page_table_root, acpi)`:**
Discovers CPUs, allocates AP stacks, starts APs. Returns total CPU count.

**SMP-API-003 — `smp::current_per_cpu()` → `&'static mut PerCpu`:**
Returns the current CPU's PerCpu struct. Panics if called before
`early_init_bsp()` (use `try_current_per_cpu()` for the safe variant).

**SMP-API-004 — `smp::cpu_count()` → `u32`:**
Returns the total number of online CPUs.

---

## Design Notes

- The AP startup sequence on x86_64: INIT → INIT de-assert → SIPI → SIPI,
  with a busy-wait for the `started` flag.
- On RISC-V: SBI `hart_start()` for each AP.
- The `PerCpu.serial_locked` field provides a per-CPU re-entrancy guard
  for the serial driver, preventing deadlock when an interrupt handler
  calls serial output while the main thread holds the global serial lock.
