# PCI Subsystem — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/pci/{mod,ecam,enumerate}.rs`
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
- Location: `kernel/src/pci/ecam.rs:12-21,23-28`

**PCI-003 — Mapped ECAM regions are searched by segment + bus:**
`find_region()` iterates the mapped list looking for `(segment, bus)`.
Returns `None` if no matching region (read returns default, write is no-op).
- Location: `kernel/src/pci/ecam.rs:85-94,98-106,108-118`

**PCI-004 — Devices are enumerated at PCI init:**
Bus 0 is scanned recursively. Each device is stored as `PciDevice`
with vendor, device, class, prog_if, and revision.
- Location: `kernel/src/pci/enumerate.rs`

---

## Safety Invariants

**PCI-S001 — ECAM volatile read/write safety:**
The mapped virtual address for `(bus, device, function, offset)` is
computed from the ECAM base + bus/device/function slot offset. The
address must be in mapped MMIO space. Volatile accesses avoid compiler
reordering.
- Location: `kernel/src/pci/ecam.rs:57-64,103,115`

**PCI-S002 — Mapped region raw pointer stability:**
`find_region()` returns `&'static MappedRegion` via `unsafe { &*(r as *const _) }`,
justified because `MAPPED` is behind a `Mutex` and the `Vec<MappedRegion>`
is never modified after initialization.
- Location: `kernel/src/pci/ecam.rs:90`

---

## API Contracts

**PCI-API-001 — `ecam::map_all(regions)`:**
Called during PCI init with MCFG regions from ACPI. Maps all ECAM
space into the kernel's page table.

**PCI-API-002 — `ecam::read_u8/u16/u32(segment, bus, dev, func, offset)`:**
Returns the config register value, or default (`0xFF`/`0xFFFF`/`0xFFFF_FFFF`)
if no matching ECAM region is found.

**PCI-API-003 — `ecam::write_u8/u16/u32(segment, bus, dev, func, offset, val)`:**
Writes config register. No-op if no matching ECAM region.

**PCI-API-004 — `ecam::read_header(segment, bus, dev, func, buf)`:**
Reads full 256-byte config header via `copy_nonoverlapping`.

---

## Design Notes

- PCI VMM uses `KERNEL_VMA_BASE - 0x10000000 - 0x20000000` as base,
  below the ACPI VMM region. 512 MB budget is generous for typical
  PCI topology.
- Enumeration only scans bus 0 (tests). Recursive bus scanning beyond
  bus 0 is not yet implemented.
- AHCI init (not PCI init) performs the PCI device scan for the AHCI
  controller's BAR.
