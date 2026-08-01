# Architecture Abstraction — Invariants

**Version:** 0.4.0
**Source:** `kernel/src/arch/mod.rs`, `kernel/src/arch/x86_64/{mod,gdt,idt,paging,trampoline}.rs`, `kernel/src/arch/riscv64/{mod,paging,sbi,trap,trampoline}.rs`
**Status:** Stable

> **Note (2026-07-31):** The compile-time `Arch` trait was removed in commit
> `c9a93b8`. It is replaced by a `CurrentArch` type alias to per-arch unit
> structs (`X86_64` / `Riscv64`) exposing **inherent** methods, plus a runtime
> capability layer (`KernelServices`) documented in `invariants-23-services.md`.
> `discover_cpus()` / `wake_aps()` moved out of `arch` into the `CpuManager`
> service (`services.cpu`).

---

## State Invariants

**ARCH-001 — `CurrentArch` resolves at compile time:**
`cfg(target_arch = "x86_64")` aliases `CurrentArch = X86_64`;
`cfg(target_arch = "riscv64")` aliases `CurrentArch = Riscv64`.
Both are unit structs with **inherent** methods — no trait object, no
`dyn` dispatch, no runtime selection.
- Location: `kernel/src/arch/mod.rs:1-9`

**ARCH-002 — `CurrentArch::init()` runs after page tables + framebuffer log, before ACPI:**
Called from `Kernel::init()` (`lib.rs:180`) after `heap::set_phys_allocator`,
`early_init_bsp`, `switch_to_higher_half`, and `enable_framebuffer_log`; it
runs **before** `init_acpi()` and `init_ioapic()`.
- Location: `kernel/src/lib.rs:173-196`

**ARCH-003 — `setup_virt_mem()` returns Vmm WITHOUT activating:**
The caller is responsible for `Vmm::activate()` after the Vmm is fully
built. This allows the caller to stash the root pointer and initialize
ACPI VMM before switching page tables.
Passes framebuffer dimensions (phys_addr, height, stride, bpp) so the
framebuffer region can be mapped with appropriate cache attributes.
- Location: `kernel/src/arch/x86_64/mod.rs:65-75`, `kernel/src/arch/riscv64/mod.rs` (`setup_virt_mem`)

**ARCH-004 — CPU discovery / wake is a `CpuManager` service, not an arch method:**
`CurrentArch` no longer provides `discover_cpus()` / `wake_aps()`. Both live
on `services::cpu::CpuManager` (`X86Cpu` on x86_64, `RiscvCpu` on riscv64)
and are reached via `kernel_services().cpu`.
- Location: `kernel/src/services/cpu.rs:8-19`, `kernel/src/services/x86_64/x86_cpu.rs`, `kernel/src/services/riscv64/riscv_cpu.rs`

**ARCH-005 — x86_64 `init()` additionally wires the universal timer:**
`X86_64::init()` calls `universal_timer::early_init(&X86TscClocksource, &ApicOneShotClockevent)`
immediately after `apic::init()` and after recording the BSP APIC ID via
`smp::set_bsp_hardware_id()`. This is before services, before SMP, before
interrupts are enabled.
- Location: `kernel/src/arch/x86_64/mod.rs:24-41`
- The clock source/event types (`X86TscClocksource`, `ApicOneShotClockevent`)
  are defined in the same file (`arch/x86_64/mod.rs:78-138`).

---

## Safety Invariants

**ARCH-S001 — `setup_virt_mem` safety:**
`allocator` must be initialised and have free frames for page-table
intermediate tables. `layout` must describe the kernel's memory sections
accurately for W^X enforcement.
- Location: `kernel/src/arch/x86_64/mod.rs:65-75`

---

## API Contracts

**ARCH-API-001 — `CurrentArch::init()`:**
Early architecture init (GDT+IDT+APIC on x86, trap vectors + PLIC on RISC-V).
Called once on the BSP after page tables are live and before ACPI/SMP.

**ARCH-API-002 — `CurrentArch::init_ap(cpu_id)`:**
Per-CPU arch init called once per AP during SMP startup.
- x86_64: `gdt::init()` → `idt::init_ap()` → `apic::init_ap()`.
  (`arch/x86_64/mod.rs:43-47`)

**ARCH-API-003 — `CurrentArch::halt()`:**
Halt the CPU (`hlt` / `wfi`). May return after interrupt or NMI.

**ARCH-API-004 — `CurrentArch::enable_interrupts()` / `disable_interrupts()`:**
Wraps the local CPU's interrupt flag (IF bit / SIE bit in sstatus).

**ARCH-API-005 — `CurrentArch::are_interrupts_enabled()` → `bool`:**
Used by `IrqMutex` and the universal timer's `IrqSafeLock` to save/restore
interrupt state. Must be accurate.

**ARCH-API-006 — `CurrentArch::setup_virt_mem(allocator, layout, stack_guard, fb_addr, fb_height, fb_stride, fb_bpp)` → `Vmm`:**
Builds identity + higher-half page tables (see `invariants-07` / `invariants-08`).

---

## Design Notes

- The old `Arch` trait separated architecture-independent kernel logic from
  platform code; the new model separates it into two layers: `CurrentArch`
  (inherent, compile-time) for low-level primitives, and `KernelServices`
  (runtime `dyn` trait objects) for higher-level capability dispatch. See
  `invariants-23-services.md`.
- All arch-specific modules live under `kernel/src/arch/<arch>/`. The
  x86_64 impl calls: `gdt::init()` → `idt::init()` → `apic::init()` →
  `universal_timer::early_init(...)`.
- The RISC-V impl calls: `trap::init()` → PLIC init → enable `sie`; it does
  **not** call `universal_timer::early_init` (riscv64 still runs the legacy
  periodic SBI 100 Hz trap timer — see `invariants-08`).
