# Virtual Memory Manager — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/mm/vmm/mod.rs`, `kernel/src/mm/vmm/x86_64.rs`, `kernel/src/mm/vmm/riscv64.rs`
**Status:** Stable

---

## State Invariants

**VMM-001 — Page-aligned mapping:**
All `map` calls require `vaddr` and `paddr` to be 4 KiB-aligned.
`map_2m` requires 2 MiB alignment. Panics on violation.
- Location: `kernel/src/mm/vmm/mod.rs:116-117,137-138`

**VMM-002 — No double-map:**
Mapping an already-mapped page panics. The arch-specific walkers assert
that the target PTE is not present before writing.
- Location: `kernel/src/mm/vmm/mod.rs:108`

**VMM-003 — `map()` auto-selects 2 MiB vs 4 KiB pages:**
When both ends are 2 MiB-aligned and `remaining >= 2 MiB`, huge pages are
used. The remainder uses 4 KiB pages.
- Location: `kernel/src/mm/vmm/mod.rs:170-184`

**VMM-004 — Higher-half alias at `KERNEL_VMA_BASE`:**
The kernel image is also mapped at `KERNEL_VMA_BASE + phys_addr` for each
4 KiB page, with identical permissions. This provides a kernel-space view
without changing the linker script.
- Location: `kernel/src/mm/vmm/mod.rs:74`

**VMM-005 — VMM manages intermediate page-table frames:**
When creating page-table entries, the arch-specific code allocates frames
from `BitmapAllocator` for intermediate tables. These frames are never freed
(mappings are permanent).
- Location: `kernel/src/mm/vmm/x86_64.rs`, `kernel/src/mm/vmm/riscv64.rs`

**VMM-006 — Identity map covers `[0, max_addr)`:**
`max_addr = max(4 GiB, fb_end, allocator.alloc_end())`, rounded up to 2 MiB.
This ensures all managed RAM, kernel image, framebuffer, and hardware-mapped
regions within range are accessible.
- Location: `kernel/src/arch/x86_64/paging.rs:36-38`

---

## Safety Invariants

**VMM-S001 — `Vmm::new` safety:**
Allocates one zeroed frame from the allocator. The frame must not be in use.
- Location: `kernel/src/mm/vmm/mod.rs:85-91`

**VMM-S002 — `Vmm::activate` safety:**
Must be called after the page table is fully built and before any code
relies on the new mappings. On x86_64, loads CR3. On RISC-V, writes SATP.
- Location: `kernel/src/mm/vmm/x86_64.rs`, `riscv64.rs`

---

## API Contracts

**VMM-API-001 — `Vmm::new(alloc)` → `Vmm`:**
Returns a `Vmm` with a single zeroed root table frame. Panics if allocator
is exhausted.

**VMM-API-002 — `Vmm::from_root(root)` → `Vmm`:**
Wraps an existing root frame (no allocation). Used by ACPI and PCI VMMs
that share the kernel page table root.

**VMM-API-003 — `Vmm::map(alloc, vaddr, paddr, size, flags)`:**
Maps a contiguous `[vaddr, vaddr+size)` to `[paddr, paddr+size)`.
All arguments must be page-aligned, `size > 0`, `size` page-aligned.
Panics on double-map or OOM for intermediate tables.

**VMM-API-004 — `Vmm::unmap(alloc, vaddr, size)`:**
Unmaps 4 KiB pages. Intermediate tables are NOT freed (leaked intentionally
for simplicity).

**VMM-API-005 — `Vmm::translate(vaddr)` → `Option<u64>`:**
Walks the page table without TLB lookups. Returns the physical address or
`None` if not mapped.

**VMM-API-006 — `PageFlags` encoding:**
`READ=1, WRITE=2, EXECUTE=4, NO_CACHE=8, USER=16`. Translated to native
PTE bits inside each arch module.
- Location: `kernel/src/mm/vmm/mod.rs:30-34`

---

## Design Notes

- The VMM is a pure page-table manipulator — it does not manage virtual
  address space allocation. Callers choose virtual addresses.
- Intermediate page-table frames are never freed (no reference counting).
  This is acceptable because mappings are generally permanent.
- ACPI and PCI subsystems maintain their own VMM states (`ACPI_STATE`,
  `PCI_VMM`) that share the same root frame and use a bump-allocated
  virtual address range below `KERNEL_VMA_BASE`.
- RISC-V uses Sv39 paging (hand-rolled, no `x86_64`-crate dependency).
