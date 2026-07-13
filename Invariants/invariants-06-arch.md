# Architecture Abstraction — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/arch/mod.rs`
**Status:** Stable

---

## State Invariants

**ARCH-001 — `CurrentArch` resolves at compile time:**
`cfg(target_arch = "x86_64")` selects `X86_64`; `cfg(target_arch = "riscv64")`
selects `Riscv64`. No runtime dispatch.
- Location: `kernel/src/arch/mod.rs:67-75`

**ARCH-002 — Arch `init()` runs before any hardware access:**
Called from `Kernel::init()` as the first step after `Kernel::new()`.
- Location: `kernel/src/lib.rs:142`

**ARCH-003 — `setup_virt_mem()` returns Vmm WITHOUT activating:**
The caller is responsible for `Vmm::activate()` after the Vmm is fully
built. This allows the caller to stash the root pointer and initialize
ACPI VMM before switching page tables.
- Location: `kernel/src/arch/mod.rs:33-48`

**ARCH-004 — `discover_cpus()` returns BSP first, APs after:**
The first entry is the BSP. Subsequent entries (if any) are APs. Each
entry is `(hardware_id, enabled)`.
- Location: `kernel/src/arch/mod.rs:52-53`

---

## Safety Invariants

**ARCH-S001 — `Arch::setup_virt_mem` safety:**
`allocator` must be initialised and have free frames for page-table
intermediate tables.
- Location: `kernel/src/arch/mod.rs:39-41`

**ARCH-S002 — `Arch::wake_aps` safety:**
`allocator` must be valid and initialised. Page tables at
`page_table_root` must be live on the BSP.
- Location: `kernel/src/arch/mod.rs:57-60`

---

## API Contracts

**ARCH-API-001 — `Arch::init()`:**
Early architecture init (GDT+IDT on x86, trap vectors on RISC-V).
Called once on the BSP before any other arch function.

**ARCH-API-002 — `Arch::init_ap(cpu_id)`:**
Per-CPU arch init called once per AP during SMP startup.

**ARCH-API-003 — `Arch::halt()`:**
Halt the CPU (hlt / wfi). May return after interrupt or NMI.

**ARCH-API-004 — `Arch::enable_interrupts()` / `disable_interrupts()`:**
Wraps the local CPU's interrupt flag (IF bit / SIE bit in sstatus).

**ARCH-API-005 — `Arch::are_interrupts_enabled()` → `bool`:**
Used by `IrqMutex` to save/restore interrupt state. Must be accurate.

---

## Design Notes

- The `Arch` trait separates architecture-independent kernel logic from
  platform code. All arch-specific modules live under `kernel/src/arch/<arch>/`.
- The x86_64 impl calls: `gdt::init()` → `idt::init()` → `apic::init()`.
- The RISC-V impl calls: `trap::init()` → `plic::init()` → enable `sie`.
