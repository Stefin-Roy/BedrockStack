# ACPI Subsystem — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/acpi/{mod,tables,madt,mcfg,fadt,gas,platform,interrupt}.rs`
**Status:** Stable

---

## State Invariants

**ACPI-001 — RSDP is parsed during `Kernel::init()`, after VMM activation (or from embedded data):**
The VMM-backed `AcpiHandler` requires live page tables for the physical-
address path. When `rsdp_data` is provided (e.g. from Multiboot2 tag),
the RSDP bytes are already in memory and `parse_tables_from_data()` is used
directly, requiring no VMM mapping for the RSDP itself.
- Location: `kernel/src/lib.rs:213-227`, `kernel/src/acpi/mod.rs`, `kernel/src/acpi/tables.rs`

**ACPI-002 — `AcpiSubsystem` stored as `Option`; ACPI is optional:**
If RSDP discovery or table parsing fails, the kernel continues without
ACPI. `reset()` and `shutdown()` are no-ops when `self.acpi` is `None`.
- Location: `kernel/src/lib.rs:65,214-227`

**ACPI-003 — Table entry signatures are validated:**
XSDT/RSDT signatures checked (`"XSDT"`/`"RSDT"`). Individual table
signatures matched to `FACP`, `APIC`, `MCFG` only (others are skipped).
Each table's checksum is verified.
- Location: `kernel/src/acpi/tables.rs:84-86,125-126,153-157,162-163`

**ACPI-004 — Table data access is bounds-checked via table `length`:**
The FADT parser checks `length >= 132` before accessing reset-reg
fields (only valid for ACPI 2.0+ tables). X_PM1a_CNT_BLK access
requires `length >= 244` (ACPI 3.0+).
- Location: `kernel/src/acpi/fadt.rs:84,94`

**ACPI-005 — ACPI VMM uses bump allocation downward from `ACPI_VADDR_BASE`:**
`map_device_mmio()` subtracts `size` from `next_vaddr` (growing downward)
and maps via the shared kernel page table root. Panics on address space
exhaustion (below `ACPI_VADDR_FLOOR`, 512 MB budget).
- Location: `kernel/src/acpi/mod.rs:21,41,44-56`

**ACPI-006 — ACPI VMM state holds a raw pointer to `BitmapAllocator`:**
The pointer is valid for the kernel's lifetime (allocator lives in `Kernel`).
All access serialized behind `Mutex<Option<AcpiVmmState>>`.
- Location: `kernel/src/acpi/mod.rs:23-32,36-38`

**ACPI-007 — PCI config regions parsed from MCFG, gracefully absent:**
`PciConfigRegions` is empty if no MCFG table found. PCI init handles
empty regions.
- Location: `kernel/src/acpi/mod.rs:110-114`

**ACPI-008 — MADT parsing prefers x2APIC entries over legacy APIC:**
On x2APIC-capable firmware, legacy type-0 entries (8-bit APIC IDs) are
stubs. Type-9 x2APIC entries (32-bit IDs) take precedence.
- Location: `kernel/src/acpi/madt.rs:164-172`

**ACPI-009 — Processor list is built from MADT:**
A flat `Vec<(local_apic_id, enabled)>` is extracted from the MADT,
bypassing the `ProcessorInfo` struct for direct use by SMP init.
- Location: `kernel/src/acpi/mod.rs:138-145`

**ACPI-010 — `parse_tables_from_data()` parses RSDP from embedded byte slice:**
When `rsdp_data: Option<&'static [u8]>` is `Some`, the function operates on
the already-mapped byte slice directly. This supports Multiboot2 tags 14 and
15 which embed the RSDP data inline. The function validates the RSDP signature,
checksum, revision, and extracts RSDT/XSDT addresses without needing a VMM
mapping call.
- Location: `kernel/src/acpi/tables.rs`

---

## Safety Invariants

**ACPI-S001 — ACPI table pointer reads (raw pointer dereferences):**
Tables are mapped via the ACPI VMM before being read as raw byte slices.
The mapped virtual address is valid for `length` bytes.
- Location: `kernel/src/acpi/tables.rs:32,37,56,74-79,96-106,134-136,145-151`

**ACPI-S002 — GAS MMIO access safety:**
`gas_read`/`gas_write` map physical addresses via the ACPI VMM before
accessing. The mapped region is covered by the page tables.
- Location: `kernel/src/acpi/gas.rs:4-11,67-91`

**ACPI-S003 — Port I/O on non-x86 architectures:**
`port_in`/`port_out` on RISC-V return 0 / are no-ops (no port I/O
space in RISC-V).
- Location: `kernel/src/acpi/gas.rs:62-65`

---

## API Contracts

**ACPI-API-001 — `AcpiSubsystem::new(rsdp_addr, rsdp_data)`:**
Parses all ACPI tables from the RSDP. `rsdp_data: Option<&'static [u8]>`
can carry embedded RSDP bytes (Multiboot2 path). When `Some`, uses
`parse_tables_from_data()` instead of mapping from physical `rsdp_addr`.
Returns `Err(AcpiError)` on bad signature, bad checksum, or missing
required tables.

**ACPI-API-002 — `AcpiSubsystem::reset()`:**
Attempts reset via:
1. FADT reset register (if `reset_supported`)
2. 8042 PS/2 controller (x86_64 only)
3. SBI cold_reboot (RISC-V only)
4. Infinite halt (ultimate fallback)

**ACPI-API-003 — `AcpiSubsystem::shutdown()`:**
Attempts S5 sleep via:
1. PM1 control registers (SLP_TYP + SLP_EN)
2. QEMU PM I/O port (x86_64 only)
3. SBI system_reset (RISC-V only)
4. Infinite halt (ultimate fallback)

---

## Design Notes

- AML interpreter (`init_aml()`) is defined but currently **disabled**
  because it hangs on QEMU. SLP_TYP for S5 defaults to `0x00` which
  works on virtual hardware.
- The ACPI table walker maps each table header (8 bytes) first to check
  `length`, then re-maps the full `length` bytes — two mapping operations
  per table.
- XSDT entries at offset 36 are not 8-byte-aligned; they are read
  byte-by-byte to avoid alignment faults.
