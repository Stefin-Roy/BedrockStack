# SMP — Invariants

**Version:** 0.3.0
**Source:** `kernel/src/smp/mod.rs`
**Status:** Stable

---

## State Invariants

**SMP-001 — `PerCpu` struct is `#[repr(C)]` with `self_ptr` as first field:**
The first field MUST be `self_ptr: *const PerCpu` pointing to itself.
On x86_64, it is accessed via `gs:[0]` (GS.base = &PerCpu). On RISC-V,
via the `tp` register. This allows `current_per_cpu()` to read the
pointer from a known offset.
The struct also carries: `cpu_id: u32`, `apic_id: u32` (hardware ID),
`is_bsp: bool`, `started: AtomicU64`, `stack_top: u64`, and
`serial_locked: AtomicU64` (re-entrancy guard).
- Location: `kernel/src/smp/mod.rs:36-45`

**SMP-002 — Maximum 16 CPUs:**
`MAX_CPUS = 16`. `PER_CPU_SLOTS` is a fixed-size array of 16 entries
with fully initialized default values (each slot has its `cpu_id` pre-set).
`per_cpu_by_id()` asserts `cpu_id < MAX_CPUS`.
- Location: `kernel/src/smp/mod.rs:48,52-69,99-102`

**SMP-003 — BSP is CPU 0, initialized at boot:**
`early_init_bsp()` sets `cpu_id = 0, apic_id = 0, is_bsp = true` and
stores 1 in `started`. Sets GS.base (x86_64) or `tp` register (RISC-V).
APs start with `started = 0` and wait for the BSP to set it to 1
(busy-wait in trampoline epilogue).
- Location: `kernel/src/smp/mod.rs:116-129`

**SMP-004 — AP stacks are 17 pages (64 KB + 4 KB guard) from `alloc_contiguous`:**
`AP_STACK_PAGES = 17`. The returned `stack_top` is the highest address
in the allocated range. The lowest page is the guard (unmapped by paging).
Allocation is done by `allocate_ap_stack()` which panics on OOM.
- Location: `kernel/src/smp/mod.rs:230-236`

**SMP-005 — `SMP init` runs after ACPI, I/O APIC, and the services container:**
`Kernel::init()` calls `smp::init()` after `init_acpi()`, `init_ioapic()`,
and `init_services()`/`set_global()` (the container is passed in directly),
but before interrupts are enabled (which happens via
`self.svc().platform.enable_interrupts()`).
- Location: `kernel/src/lib.rs:183-217`

**SMP-006 — CPU_COUNT reflects total online CPUs:**
Stored as `AtomicU32`, set during `smp::init()` before APs are started.
Also readable via `cpu_count()`.
- Location: `kernel/src/smp/mod.rs:50,104-106,210`

**SMP-007 — AP `started` flag is an `AtomicU64` in `PerCpu`:**
The trampoline code polls this address until it becomes non-zero.
The BSP writes 1 after the AP completes its initialization.
- Location: `kernel/src/smp/mod.rs:44,122,196`

**SMP-008 — `ApReady` cache-line-aligned ready flags:**
`AP_READY: [ApReady; MAX_CPUS]` is a static array of `AtomicBool`
flags, each cache-line-aligned (`#[repr(align(64))]`) to prevent
false sharing between CPUs during the AP startup handshake.
- Location: `kernel/src/smp/mod.rs:7-29`

**SMP-009 — `ApContext` carries per-AP wake data:**
`ApContext { cpu_id, hardware_id, stack_top }` is built during
`smp::init()` for each discovered AP. The list is passed to
`services.cpu.wake_aps()` (via the `CpuManager` capability) for arch-specific
startup (IPIs / SBI ecalls) — no longer an `Arch` method.
- Location: `kernel/src/smp/mod.rs:158-162,178,219`

**SMP-010 — `current_cpu_id()` returns the current CPU's ID:**
Convenience wrapper around `current_per_cpu().cpu_id`.
- Location: `kernel/src/smp/mod.rs:106-108`

**SMP-011 — `set_bsp_hardware_id(id)` fills BSP's APIC/hart ID:**
Called from `X86_64::init()` immediately after `apic::init()` (recording
`apic::read_full_apic_id()`), so the PerCpu slot is populated before
`discover_cpus()` runs. AP hardware IDs are set during `smp::init()`.
- Location: `kernel/src/arch/x86_64/mod.rs:33`, `kernel/src/smp/mod.rs:142-144`

**SMP-012 — `find_cpu_by_hardware_id(hw_id)` maps hardware ID to PerCpu:**
Scans all 16 slots, returns `Some((&mut PerCpu, cpu_id))` on match.
Used by interrupt handlers that receive hardware (APIC/hart) IDs.
- Location: `kernel/src/smp/mod.rs:149-157`

---

## Safety Invariants

**SMP-S001 — `early_init_bsp()` safety:**
Must be called exactly once on the BSP before any SMP operations.
Writes to `PER_CPU_SLOTS[0]` (static mut) and sets GS.base / tp.
- Location: `kernel/src/smp/mod.rs:116-129`

**SMP-S002 — `smp::init()` safety:**
Must be called after heap, page tables, ACPI, I/O APIC init, and the
services container (`init_services`/`set_global`). Allocates AP stacks from
the physical allocator. Executes `services.cpu.wake_aps()` which issues
IPIs/SBI calls. Collects APs in a `Vec<ApContext>` (requires heap).
- Location: `kernel/src/smp/mod.rs:170-226`

**SMP-S003 — `current_per_cpu()` safety (x86_64):**
Reads `gs:[0]` via inline asm. GS.base must have been set by
`early_init_bsp()` (or by the AP trampoline), and `self_ptr` must
point to the correct `PerCpu` slot.
- Location: `kernel/src/smp/mod.rs:71-78`

**SMP-S004 — `current_per_cpu()` safety (RISC-V):**
Reads the `tp` register. Must have been set by `early_init_bsp()` or
AP trampoline.
- Location: `kernel/src/smp/mod.rs:80-87`

**SMP-S005 — `try_current_per_cpu()` safe variant:**
Checks if `PER_CPU_SLOTS[0].self_ptr` is null. If so, returns `None`
(not yet initialized). Otherwise delegates to `current_per_cpu()`.
- Location: `kernel/src/smp/mod.rs:89-97`

---

## API Contracts

**SMP-API-001 — `smp::early_init_bsp()`:**
Initializes PerCpu slot for BSP. Called early in `Kernel::init()`.
No return value.

**SMP-API-002 — `smp::init(page_table_root, acpi, services)`:**
Discovers CPUs via `services.cpu.discover_cpus(acpi)`, allocates AP stacks,
configures PerCpu slots for each AP, builds `Vec<ApContext>`, calls
`services.cpu.wake_aps(page_table_root, &ap_list)`, returns total CPU count
(`u32`). The `allocator` parameter was removed in the services refactor
(`c9a93b8`); the allocator is reached via `heap::get_phys_allocator_mut()`.

**SMP-API-003 — `smp::current_per_cpu()` → `&'static mut PerCpu`:**
Returns the current CPU's PerCpu struct. Panics if called before
`early_init_bsp()` (use `try_current_per_cpu()` for the safe variant).

**SMP-API-004 — `smp::try_current_per_cpu()` → `Option<&'static mut PerCpu>`:**
Returns `None` before `early_init_bsp()` has been called. Used by the
serial driver to decide whether to take per-CPU locks.

**SMP-API-005 — `smp::cpu_count()` → `u32`:**
Returns the total number of online CPUs.

**SMP-API-006 — `smp::current_cpu_id()` → `u32`:**
Returns the current CPU's numeric ID (`cpu_id`).

**SMP-API-007 — `smp::per_cpu_by_id(cpu_id)` → `&'static mut PerCpu`:**
Returns a reference to the PerCpu slot for the given CPU ID.
Asserts `cpu_id < MAX_CPUS`.

**SMP-API-008 — `smp::set_bsp_hardware_id(id)`:**
Sets `PER_CPU_SLOTS[0].apic_id` to the BSP's hardware ID. Called from
`X86_64::init()` right after `apic::init()`.

**SMP-API-009 — `smp::find_cpu_by_hardware_id(hw_id)` → `Option<(&mut PerCpu, u32)>`:**
Scans all CPU slots for matching hardware ID. Returns `None` if not found.

---

## Design Notes

- The AP startup sequence on x86_64: INIT → INIT de-assert → SIPI → SIPI,
  with a busy-wait for the `started` flag.
- On RISC-V: SBI `hart_start()` for each AP.
- CPU discovery and wake now live on the `CpuManager` capability
  (`services.cpu`), not on the arch layer. `smp::init()` receives the
  already-built `KernelServices` container.
- The `PerCpu.serial_locked` field provides a per-CPU re-entrancy guard
  for the serial driver, preventing deadlock when an interrupt handler
  calls serial output while the main thread holds the global serial lock.
- `AP_READY` cache-line-aligned flags prevent false sharing during the
  spin-up handshake. Each AP sets its flag after completing trampoline init,
  and the BSP waits on these flags rather than using a fixed delay.
- `PER_CPU_SLOTS` is a `static mut` array with all 16 slots pre-initialized
  (each with distinct `cpu_id` values 0–15), avoiding the need for dynamic
  allocation during SMP init.
