# Kernel Services (Capability Layer) — Invariants

**Version:** 0.1.1
**Date:** 2026-08-01
**Source:** `kernel/src/services/{mod,cpu,dma,interrupts,msi,null_msi,pci_config,ecam_pci_config,pci_device,phys_mem,platform,serial,timer,virt_mem,block_device,acpi,clocksource,clockevent,timer_queue,universal_timer}.rs`, `kernel/src/services/x86_64/*`, `kernel/src/services/riscv64/*`
**Status:** Stable

> **Note:** This subsystem was introduced by commit `c9a93b8`, which replaced
> the compile-time `Arch` trait with a runtime container of capability trait
> objects. `invariants-06-arch.md` documents the remaining `CurrentArch`
> layer; this file documents the `KernelServices` container.

---

## State Invariants

**SVC-001 — `KernelServices` is an immutable container of ten `&'static dyn` capability objects:**
Fields: `timer`, `interrupts`, `serial`, `platform`, `cpu`, `pci_cfg`,
`msi`, `pci`, `acpi` (`Option<...>`), `dma`. Built once during
`Kernel::init()`, never mutated afterwards.
- Location: `kernel/src/services/mod.rs:44-55`

**SVC-002 — The container is built after arch init + ACPI + IOAPIC, before SMP:**
`Kernel::init()` constructs it via `init_services(...)`, `Box::leak`s it to
`'static`, stores it in `Kernel.services`, then calls
`set_global()` before `smp::init()`.
- Location: `kernel/src/lib.rs:198-212`

**SVC-003 — The global reference is set exactly once via `spin::Once`:**
`set_global()` uses `GLOBAL_SERVICES.call_once()`. `kernel_services()`
panics if called before `set_global`.
- Location: `kernel/src/services/mod.rs:76-90`

**SVC-004 — Platform selection is confined to `init_services()`:**
`#[cfg(target_arch)]` picks `x86_services()` or `riscv_services()`; every
consumer uses `dyn` dispatch and needs no `cfg` gates.
- Location: `kernel/src/services/mod.rs:62-72`

**SVC-005 — Every capability provider is `Send + Sync`:**
P2 removed the legacy `Capability` supertrait (and its file,
`services/capability.rs`); every provider now implements the container's
trait objects as `Send + Sync` directly. The `dyn` capability traits
declared in `services/mod.rs` require `Send + Sync` supertraits, so all
providers are thread-safe and immutable — the container can be shared across
CPUs via a `'static` leak.
- Location: `kernel/src/services/mod.rs:44-55`

**SVC-006 — `platform`, `cpu`, `pci_cfg`, and `dma` are consumed today:**
`lib.rs` uses `svc().platform.halt()/enable_interrupts()`;
`smp::init()` uses `services.cpu.discover_cpus()/wake_aps()`;
`pci/{caps,enumerate,msi,msix}.rs` route config access through
`kernel_services().pci_cfg`; the AHCI (blockdriver) and xHCI (USB)
storage inits allocate all DMA through `kernel_services().dma`. The
remaining fields are populated but not yet consumed by hot paths.
- Location: `kernel/src/lib.rs:217,389`, `kernel/src/smp/mod.rs:178,219`, `kernel/src/pci/msix.rs`, `kernel/src/filesystems/blockdriver/driver.rs:32-72`, `kernel/src/usb/xhci/mod.rs:27-60`

**SVC-007 — DMA is a global singleton (`Once<KernelDma>`), the only DMA allocator:**
`init_dma_allocator(root, alloc)` populates `DMA_ALLOCATOR` once. `KernelDma`
owns a 512 MiB VMM region below `KERNEL_VMA_BASE - 0x5000_0000`, grows
allocations downward, and maps everything `NO_CACHE | READ | WRITE`.
AHCI and xHCI both share this allocator via `kernel_services().dma`.
- Location: `kernel/src/services/dma.rs:23-30,97-159`

**SVC-008 — The shared translation cache is mutex-protected and sized 64:**
`TRANS_CACHE: Mutex<TransCacheInner>` caches `(vaddr_page, phys)` pairs;
`virt_to_phys()` consults it before walking page tables.
- Location: `kernel/src/services/dma.rs:32-59`

---

## Service Matrix

| Trait | Provider(s) | Notes |
|---|---|---|
| `UniversalTimer` | `UniversalTimerImpl` (x86 + riscv wiring; riscv path currently unwired) | one-shot deadlines over a min-heap queue + clocksource; see `invariants-13` |
| `InterruptManager` | `X86Interrupts` (x86), `RiscvInterrupts` (riscv) | x86 wraps `idt::register_device_handler_at` + `apic_eoi`; riscv has its own `PLIC_HANDLERS[127]` |
| `SerialConsole` | `KernelSerial` (shared) | delegates to `drivers::serial::SerialPort` |
| `PlatformControl` | `X86Platform`, `RiscvPlatform` | shutdown/reset/halt/interrupt flag control |
| `CpuManager` | `X86Cpu`, `RiscvCpu` | discovery + AP wake + IPI; `discover_cpus` moved here from arch |
| `PciConfigSpace` | `EcamPciConfig` (both arches) | read8/16/32, write8/16/32 via `pci::ecam` |
| `MsiAllocator` | `X86Msi`, `NullMsi` (riscv no-op) | vector allocation + message address/data |
| `PciDeviceManager` | `X86PciDevice`, `RiscvPciDevice` (stub) | device list, BARs, capabilities, MSI/MSI-X programming |
| `AcpiProvider` | `X86Acpi` (wraps `&AcpiSubsystem`), `RiscvAcpi` (stub) | interrupt model, config regions, cpus, MMIO mapping |
| `DmaAllocator` | `KernelDma` | `alloc_page`/`alloc_contiguous`/`map_mmio`/`virt_to_phys` |

---

## Dead / Orphaned Traits

**SVC-D001 — `TimerProvider` (`services/timer.rs`) is dead code:**
`ApicTimer` (x86) and `SbiTimer` (riscv) still exist on disk but nothing
references them; the universal timer superseded them. Do not confuse with
`UniversalTimer`.
- Location: `kernel/src/services/timer.rs`, `kernel/src/services/x86_64/apic_timer.rs`, `kernel/src/services/riscv64/sbi_timer.rs`

**SVC-D002 — `services::block_device::BlockDevice` is a dead duplicate:**
The active `BlockDevice` trait lives at
`filesystems::blockdriver::traits::BlockDevice` (with `IoBuffer`/`IoRequest`/
`IoCompletions`). The services copy adds `Capability` but has zero impls and
is referenced nowhere.
- Location: `kernel/src/services/block_device.rs`

**SVC-D003 — `VirtualMemoryManager` (`services/virt_mem.rs`) is unimplemented:**
`Vmm` does not implement it because `map`/`unmap` require a
`&mut BitmapAllocator` parameter. Documented in `vmm/mod.rs`; deferred until
the allocator is stored inside `Vmm`.
- Location: `kernel/src/mm/vmm/mod.rs`, `kernel/src/services/virt_mem.rs`

**SVC-D004 — `PhysicalMemoryAllocator` (`services/phys_mem.rs`) is implemented by `BitmapAllocator`:**
`alloc_frames(count)` → `alloc()` (count 1) or `alloc_contiguous(count)`;
`free_frames(addr, _count)` **ignores `count`** — only the frame at `addr` is
freed. `BitmapAllocator` is `Send + Sync` (externally synchronized).
- Location: `kernel/src/mm/phys_alloc.rs:334-365`

---

## API Contracts

**SVC-API-001 — `init_services(root: u64, alloc: *mut BitmapAllocator, acpi: Option<&'static AcpiSubsystem>) -> KernelServices`:**
Builds the container. `alloc` is the `&mut BitmapAllocator` inside `Kernel`
taken as a raw pointer and must stay valid for the kernel's lifetime.
- Location: `kernel/src/services/mod.rs:62-72`

**SVC-API-002 — `set_global(&'static KernelServices)` / `kernel_services() -> &'static KernelServices`:**
Global accessor for driver hot paths. Must be called after construction;
panics before that.
- Location: `kernel/src/services/mod.rs:81-89`

**SVC-API-003 — `CpuManager::discover_cpus(acpi) -> Vec<(u32, bool)>`:**
Returns `(hardware_id, enabled)` with the BSP first. Replaces `Arch::discover_cpus`.
- Location: `kernel/src/services/cpu.rs:13`

**SVC-API-004 — `CpuManager::wake_aps(page_table_root, &[ApContext]) -> usize`:**
Issues arch-specific AP startup (IPIs / SBI hart_start). Replaces `Arch::wake_aps`.
- Location: `kernel/src/services/cpu.rs:14-18`

**SVC-API-005 — `PciConfigSpace` read/write with no-region defaults:**
`read8/16/32` return `0xFF/0xFFFF/0xFFFFFFFF` and writes are no-ops when no
ECAM region matches `(segment, bus)`. All `pci/{caps,enumerate,msi,msix}`
code routes through `kernel_services().pci_cfg`.
- Location: `kernel/src/services/pci_config.rs:3-10`, `kernel/src/services/ecam_pci_config.rs`

---

## Design Notes

- The `'static` lifetime comes from `Box::leak` of a single container
  instance; providers are mostly `static` unit structs, with two `spin::Once`
  singletons (`UNIVERSAL_TIMER`, `DMA_ALLOCATOR`).
- x86_64 wiring order inside `x86_services()`: `universal_timer()` →
  `x86_interrupts::init()` → `x86_serial::init()` → `x86_platform::init()` →
  `x86_cpu::init()` → `ecam_pci_config::init()` → `x86_msi::init()` →
  `x86_pci_device::init()` → `X86Acpi` (leaked) → `init_dma_allocator()`.
- riscv64 wiring order inside `riscv_services()`: `universal_timer()` →
  `riscv_interrupts::init()` → shared `serial::init()` → `riscv_platform::init()`
  → `riscv_cpu::init()` → `ecam_pci_config::init()` → `null_msi::init()` →
  `riscv_acpi::init()` → `init_dma_allocator()`.
- **riscv64 caveat:** `Riscv64::init()` never calls
  `universal_timer::early_init`, yet `riscv_services()` calls
  `universal_timer()` which `expect`s the `Once`. The riscv64 path is
  currently unwired and would panic if reached; riscv64 still runs the legacy
  periodic SBI 100 Hz trap timer.
- DMA is a single allocator: `services::dma::KernelDma` (exposed as
  `KernelServices.dma`), shared by AHCI (`blockdriver`) and xHCI (`usb`).
  All three formerly-separate allocators (`services::dma::KernelDma`,
  `filesystems::blockdriver::dma::DmaAllocator`, `usb::dma::UsbDmaAllocator`)
  were unified into it. It lives in the former AHCI carve-out
  (`KERNEL_VMA_BASE - 0x5000_0000`, 512 MiB), directly below the PCI ECAM
  window (`KERNEL_VMA_BASE - 0x3000_0000`) so the two never overlap.
