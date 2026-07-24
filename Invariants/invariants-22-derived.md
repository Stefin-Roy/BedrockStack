# Derived Properties — Invariants

**Version:** 0.2.0
**Description:** Higher-level correctness properties that follow from the
subsystem invariants in files `invariants-01` through `invariants-21`.

---

## Memory Safety

**DERV-001 — No double-alloc follows from ALLOC-001:**
Each frame has exactly one bit. `alloc()` requires the bit to be 0,
then sets it to 1. A second `alloc()` for the same frame cannot succeed
because the bit is already 1.

**DERV-002 — No double-free follows from ALLOC-001:**
`free()` clears the bit to 0, irrespective of the prior state. A
double-free clears an already-0 bit, causing no memory corruption.
However, it may lead to a frame being allocated while freed → memory
corruption. The `# Safety` on `free()` requires the caller to guarantee
the frame is allocated.

**DERV-003 — No alloc of reserved frames follows from ALLOC-003 + ALLOC-004:**
Reserved frames (kernel image, bitmap, MMIO, firmware) are never in the
`Usable` regions, so they are never cleared to 0 in the bitmap. `alloc()`
only returns frames from cleared regions.

**DERV-004 — W^X security follows from PAGING-003 + RISCV-002:**
Page permissions are set at build time based on section membership.
NXE (x86) and X-bit (RISC-V) prevent execution of writable pages.

**DERV-005 — NULL dereference faults follow from PAGING-004 + RISCV-003:**
Frame 0 is unmapped. Any load/store through a NULL pointer hits an
unmapped page and generates a page fault (#PF on x86, page-fault on
RISC-V).

**DERV-006 — Stack overflow faults (not silent corruption) follows from
PAGING-005 + RISCV-003 + GDT-001:**
The guard page is unmapped, so overflowing the stack hits a fault.
On x86_64, the double-fault handler's IST provides a valid stack,
preventing triple-fault.

**DERV-007 — ACPI VMM cannot exhaust virtual space on real hardware:
512 MB budget (ACPI_VADDR_BASE to ACPI_VADDR_FLOOR) is sufficient for
all ACPI tables on any real system (typical RSDP + XSDT + 10-20 tables
< 1 MB).

**DERV-008 — No serial deadlock follows from SERIAL-001 + SERIAL-003:**
The two-level lock (per-CPU then global) prevents re-entrancy deadlock:
if an interrupt handler on the same CPU tries to print, it spins on the
per-CPU lock already held by the interrupted main thread.

**DERV-009 — FD table operations are safe under interrupts:
`FdTable` is protected by `IrqMutex`, which disables interrupts during
critical sections (VFS-001).

**DERV-010 — No stale ECAM references follow from PCI-003:**
Mapped ECAM regions are `Vec<MappedRegion>` stored behind a `Mutex`
and never modified after init. The `&'static` cast in `find_region()` is
safe because the data is pinned.

---

## System Correctness

**DERV-011 — Kernel receives a valid memory map follows from BOOT-002:**
The memory map is extracted AFTER `exit_boot_services()` from the
authoritative final map. All usable memory is correctly identified.
Capacity over-provision prevents truncation.

**DERV-012 — Kernel can always boot follows from ACPI-002:**
ACPI is optional. If RSDP is missing (0) or table parsing fails, the
kernel continues without power management or interrupt model info.
Fallbacks exist for reset/shutdown.

**DERV-013 — S5 shutdown always halts follows from ACPI-API-003:**
Every shutdown path either succeeds or falls through to an infinite
halt loop. The CPU never executes unknown code after shutdown.

**DERV-014 — PCI config reads never fault follow from PCI-002:**
If no ECAM region matches `(segment, bus)`, `read_*` returns a default
value and `write_*` is a no-op. No MMIO access occurs.

**DERV-015 — AP startup timeout is bounded follows from SMP-007:**
APs busy-wait on an atomic flag. The BSP writes 1 after init. If an
AP never starts (e.g., SIPI lost), the BSP does NOT hang — it records
the started count and continues.

---

## Cross-Subsystem Invariants

**DERV-016 — All `#[repr(C)]` types shared between boot and kernel
must have identical definitions (COMMON-001):**
`MemoryRegion`, `FramebufferInfo`, `PixelFormat`, `MemoryRegionKind`
live in the `common` crate used by both binaries.

**DERV-017 — The hand-off register ABI (BOOT-S003) must match the
kernel entry point expectation (kernel/src/main.rs):**
x86_64: `rdi=regions, rsi=regions_len, rdx=fb, rcx=stack_guard, r8=rsdp_addr`.

**DERV-018 — The physical memory map identity coverage must include
all frames the heap might allocate (VMM-006 + ALLOC-006):**
x86_64 (`ram_end = alloc_end().max(apic_base + PAGE_4K)`) and RISC-V
(`max_addr = fb_end.max(alloc_end())`) guarantee that any frame
`BitmapAllocator::alloc()` can return is identity-mapped. No hardcoded
4 GiB minimum. Framebuffer pages above `ram_end` are mapped as a
separate identity extension with appropriate cache attributes.
