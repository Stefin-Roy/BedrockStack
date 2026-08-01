# PCI Subsystem — Invariants

**Version:** 0.4.0
**Date:** 2026-07-31
**Source:** `kernel/src/pci/{mod,ecam,enumerate}.rs`, `kernel/src/services/{pci_config,ecam_pci_config,pci_device}.rs`
**Status:** Stable

---

## State Invariants

**PCI-001 — ECAM regions are mapped before access:**
`map_all()` calls `map_ecam()` for each MCFG region, allocating virtual
address space below `PCI_VADDR_BASE` (512 MB budget). Panics on exhaustion.
- Location: `kernel/src/pci/ecam.rs:8-10,31-43,69-83`

**PCI-002 — ECAM VMM state holds a raw pointer to `BitmapAllocator`:**
Same pattern as ACPI VMM. The pointer is valid for the kernel's lifetime.
Access serialized behind `Mutex<Option<PciVmmState>>`.
- Location: `kernel/src/pci/ecam.rs:12-21,23-29`

**PCI-003 — Mapped ECAM regions are searched by segment + bus:**
`find_region()` iterates the mapped list looking for `(segment, bus)`
with a bus-range `contains()` check. Returns `None` if no matching region
(read returns default, write is no-op). The virtual address is computed as
`virt_base | (bus << 20) | (device << 15) | (function << 12) | offset`.
- Location: `kernel/src/pci/ecam.rs:45-65,85-94`

**PCI-004 — Devices are enumerated recursively at PCI init:**
`pci::init(regions, root, alloc)` maps all ECAM regions then calls
`enumerate::enumerate(0)` (segment group 0). Bus 0 is scanned; PCI-PCI
bridges (`class 0x06, subclass 0x04`) are followed recursively via their
secondary-bus register. Multi-function devices (header type bit 7) are
scanned per-function. Each device is stored as `PciDevice` with vendor,
device, revision, class, subclass, prog_if, all 6 BARs, `bars_consumed`
bitmask, caps pointer, and interrupt line/pin.
- Location: `kernel/src/pci/mod.rs:40-52`, `kernel/src/pci/enumerate.rs:17-46,48-97`

**PCI-005 — Discovered devices are leaked as a static slice:**
`enumerate()` leaks the boxed slice (`DEVICES: Option<&'static [PciDevice]>`);
`pci::devices()` returns it. Set once, never freed.
- Location: `kernel/src/pci/enumerate.rs:11,17-22`, `kernel/src/pci/mod.rs:55-57`

**PCI-006 — Config-space access routes through the `PciConfigSpace` service:**
`EcamPciConfig` (unit struct, capability `"ecam-pci-config"`) implements
`PciConfigSpace` and forwards `read8/16/32`/`write8/16/32` to
`pci::ecam::read_*`/`write_*`. It is installed as `KernelServices.pci_cfg`
and used by `enumerate` and the `PciDeviceManager` impls. riscv64 shares the
same ECAM provider.
- Location: `kernel/src/services/ecam_pci_config.rs:5-43`, `kernel/src/services/mod.rs:50`
- Routing: `kernel/src/pci/enumerate.rs:6-8` (`kernel_services().pci_cfg`)

**PCI-007 — `PciDeviceManager` capability wraps device discovery + config:**
Provides `devices()`, `bar()`, `find_capability()`/`has_capability()`,
`cfg()` (returns the `PciConfigSpace`), MSI/MSI-X `configure_msi`/
`configure_msix`/`program_msix_entry`/`disable_msi`/`disable_msix`,
`msix_table_info()`, and `read_config_u8/16/32`/`write_config_u8/16/32`
accessors. `X86PciDevice` and `RiscvPciDevice` implement it.
- Location: `kernel/src/services/pci_device.rs:8-51`, `kernel/src/services/x86_64/x86_pci_device.rs:13-98`, `kernel/src/services/riscv64/riscv_pci_device.rs:13-71`

---

## Safety Invariants

**PCI-S001 — ECAM volatile read/write safety:**
The mapped virtual address for `(bus, device, function, offset)` is
computed from the ECAM base + bus/device/function slot offset. The
address must be in mapped MMIO space. Volatile accesses avoid compiler
reordering.
- Location: `kernel/src/pci/ecam.rs:57-64,96-118`

**PCI-S002 — Mapped region raw pointer stability:**
`find_region()` returns `&'static MappedRegion` via `unsafe { &*(r as *const _) }`,
justified because `MAPPED` is behind a `Mutex` and the `Vec<MappedRegion>`
is never modified after initialization.
- Location: `kernel/src/pci/ecam.rs:85-94`

---

## API Contracts

**PCI-API-001 — `ecam::map_all(regions)`:**
Called during PCI init with MCFG regions from ACPI. Maps all ECAM
space into the kernel's page table. Must follow `ecam::init_vmm(root, alloc)`.

**PCI-API-002 — `ecam::read_u8/u16/u32(segment, bus, dev, func, offset)`:**
Returns the config register value, or default (`0xFF`/`0xFFFF`/`0xFFFF_FFFF`)
if no matching ECAM region is found.

**PCI-API-003 — `ecam::write_u8/u16/u32(segment, bus, dev, func, offset, val)`:**
Writes config register. No-op if no matching ECAM region.

**PCI-API-004 — `ecam::read_header(segment, bus, dev, func, buf)`:**
Reads full 256-byte config header via `copy_nonoverlapping`. Leaves `buf`
untouched if no matching region.
- Location: `kernel/src/pci/ecam.rs:128-137`

---

## Design Notes

- PCI VMM uses `PCI_VADDR_BASE = KERNEL_VMA_BASE - 0x10000000 - 0x20000000`
  as base, below the ACPI VMM region. 512 MB budget is generous for typical
  PCI topology.
- Enumeration scans **segment group 0** only; multi-segment support can be
  added by iterating `regions.regions` for unique segment groups. Bridge
  recursion covers secondary buses within that segment.
- `pci::init()` performs the full enumeration at kernel init; AHCI (and USB)
  drivers find their controllers via `pci::devices()`, not a separate scan.
- BAR parsing tracks 64-bit BARs: if a BAR is memory-space (`bit 0` clear)
  with `type == 4` (64-bit), the next slot's bit is set in `bars_consumed`.
