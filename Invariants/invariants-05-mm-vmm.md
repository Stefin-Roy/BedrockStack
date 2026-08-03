# Virtual Memory Manager — Invariants

**Version:** 0.4.0
**Date:** 2026-07-31
**Source:** `kernel/src/mm/vmm/mod.rs`, `kernel/src/mm/vmm/x86_64.rs`, `kernel/src/mm/vmm/riscv64.rs`
**Status:** Stable

---

## State Invariants

**VMM-001 — Page-aligned mapping:**
`map_4k` requires `vaddr` and `paddr` to be 4 KiB-aligned. `map_2m` requires
2 MiB alignment. `map()` delegates to both, auto-selecting page size. Panics
on misalignment.
- Location: `kernel/src/mm/vmm/mod.rs:114-148`

**VMM-002 — No double-map:**
Mapping an already-mapped page panics. The arch-specific walkers assert
that the target PTE is not present before writing.
- Location: `kernel/src/mm/vmm/mod.rs:112`

**VMM-003 — `map()` auto-selects 2 MiB vs 4 KiB pages:**
When both ends are 2 MiB-aligned and `remaining >= 2 MiB`, huge pages are
used via `map_2m`. The remainder uses 4 KiB pages via `map_4k`.
- Location: `kernel/src/mm/vmm/mod.rs:158-190`

**VMM-004 — Higher-half alias at `KERNEL_VMA_BASE` (0xFFFFFF8000000000):**
The kernel image is mapped at `KERNEL_VMA_BASE + phys_addr` for each
4 KiB page, with identical permissions. This provides a kernel-space view
without changing the linker script.
- Location: `kernel/src/mm/vmm/mod.rs:79`, `kernel/src/arch/x86_64/paging.rs:129-134`

**VMM-005 — VMM manages intermediate page-table frames:**
When creating page-table entries, the arch-specific code allocates frames
from `BitmapAllocator` for intermediate tables. On x86_64, `unmap_4k` now
reclaims any intermediate table whose 512 entries are all non-present after a
leaf is unmapped, returning those frames to the allocator (bottom-up: L1 →
L2 → L3; the PML4 root is never freed). The caller-owned leaf frame is never
freed by the VMM. RISC-V `unmap_4k` currently keeps the leaf-clear-only
behaviour.
- Location: `kernel/src/mm/vmm/x86_64.rs`, `kernel/src/mm/vmm/riscv64.rs`

**VMM-006 — `flush_tlb` is a free function:**
TLB flush is exposed both as `Vmm::flush_tlb()` and as the arch-agnostic
free function `vmm::flush_tlb()` (reloads CR3 on x86_64, `sfence.vma` on
RISC-V). `unmap_4k` performs a full flush after reclaiming intermediate
tables, since freeing them can drop more than the single unmapped page's
translation.
- Location: `kernel/src/mm/vmm/mod.rs:245-268`, `kernel/src/mm/vmm/x86_64.rs`

**VMM-006 — Identity map covers `[0, ram_end)`, framebuffer extension beyond:**
`ram_end = alloc_end().max(apic_base + PAGE_4K)` (x86_64) or
`ram_end = alloc_end().max(fb_end)` (RISC-V), rounded up to 2 MiB.
No hardcoded 4 GiB minimum. If the framebuffer sits above `ram_end`, it is
identity-mapped as a separate extension with `WRITE_COMBINING` (x86_64) or
`NO_CACHE` (RISC-V).
- Location: `kernel/src/arch/x86_64/paging.rs:55-58`, `kernel/src/arch/riscv64/paging.rs`

**VMM-007 — `init_pat_wc` and `make_read_only_both` are re-exported:**
`init_pat_wc()` (programs PAT MSR entry 1 as WC) is re-exported at
`vmm::init_pat_wc`. `make_read_only_both()` is re-exported at
`vmm::make_read_only_both` for making kernel pages read-only in both
identity and higher-half mappings.
- Location: `kernel/src/mm/vmm/mod.rs:14,16`

---

## Safety Invariants

**VMM-S001 — `Vmm::new` safety:**
Allocates one zeroed frame from the allocator. The frame must not be in use.
- Location: `kernel/src/mm/vmm/mod.rs:90-97`

**VMM-S002 — `Vmm::activate` safety:**
Must be called after the page table is fully built and before any code
relies on the new mappings. On x86_64, loads CR3. On RISC-V, writes SATP.
- Location: `kernel/src/mm/vmm/x86_64.rs`, `riscv64.rs`

---

## API Contracts

**VMM-API-001 — `Vmm::new(alloc)` → `Vmm`:**
Returns a `Vmm` with a single zeroed root table frame. Panics if allocator
is exhausted.
- Location: `kernel/src/mm/vmm/mod.rs:90-97`

**VMM-API-002 — `Vmm::from_root(root)` → `Vmm`:**
Wraps an existing root frame (no allocation). Used by ACPI and PCI VMMs
that share the kernel page table root.
- Location: `kernel/src/mm/vmm/mod.rs:100-102`

**VMM-API-003 — `Vmm::map(alloc, vaddr, paddr, size, flags)`:**
Maps a contiguous `[vaddr, vaddr+size)` to `[paddr, paddr+size)`.
All arguments must be page-aligned, `size > 0`, `size` page-aligned.
Panics on double-map or OOM for intermediate tables. Auto-selects 2 MiB
vs 4 KiB pages.
- Location: `kernel/src/mm/vmm/mod.rs:158-190`

**VMM-API-004 — `Vmm::map_4k(alloc, vaddr, paddr, flags)`:**
Maps a single 4 KiB page. Panics if already mapped or OOM.
- Location: `kernel/src/mm/vmm/mod.rs:114-127`

**VMM-API-005 — `Vmm::map_2m(alloc, vaddr, paddr, flags)`:**
Maps a single 2 MiB huge page. Panics on alignment violation or OOM.
- Location: `kernel/src/mm/vmm/mod.rs:135-148`

**VMM-API-006 — `Vmm::unmap(alloc, vaddr, size)`:**
Unmaps 4 KiB pages. On x86_64, intermediate tables that become empty are
reclaimed (their frames returned to `alloc`); the mapped page's own frame is
left to the caller. `unmap_4k()` unmaps a single page, returning `false` if
not mapped. `unmap()` performs a full TLB flush after any reclamation.
- Location: `kernel/src/mm/vmm/mod.rs:197-232`

**VMM-API-007 — `Vmm::translate(vaddr)` → `Option<u64>`:**
Walks the page table without TLB lookups. Returns the physical address or
`None` if not mapped.
- Location: `kernel/src/mm/vmm/mod.rs:220-225`

**VMM-API-008 — `Vmm::flush_tlb()`:**
Flushes the TLB for the whole address space (reloads CR3 on x86_64).
Called after page-table mutation by the kernel VMM consumer.
- Location: `kernel/src/mm/vmm/mod.rs:228-230`

**VMM-API-009 — `PageFlags` encoding:**
`READ=1, WRITE=2, EXECUTE=4, NO_CACHE=8, USER=16, WRITE_COMBINING=32`.
Translated to native PTE bits inside each arch module.
On x86_64, `WRITE_COMBINING` sets `PWT=1, PCD=0, PAT=0` (PAT index 1),
requiring `IA32_PAT` MSR entry 1 to be programmed as `01h` (WC) via
`init_pat_wc()` before any such mapping is created.
- Location: `kernel/src/mm/vmm/mod.rs:33-39`, `kernel/src/mm/vmm/x86_64.rs`

---

## Design Notes

- The VMM is a pure page-table manipulator — it does not manage virtual
  address space allocation. Callers choose virtual addresses.
- On x86_64, empty intermediate page-table frames are returned to the
  allocator on `unmap` (bottom-up reclamation). RISC-V retains the older
  leaf-clear-only behaviour; parity is intended as a follow-up.
- The TLB flush performed after unmap/reclaim is local-core only. The page
  table root is shared across APs, so a full cross-core flush (IPI-based) is
  not yet implemented; this matches the pre-existing local-only `flush_tlb`.
  In practice the default boot runs without APs.
- ACPI and PCI subsystems maintain their own VMM states (`ACPI_STATE`,
  `PCI_VMM`) that share the same root frame and use a bump-allocated
  virtual address range below `KERNEL_VMA_BASE`.
- RISC-V uses Sv39 paging (hand-rolled, no `x86_64`-crate dependency).
- On x86_64, `WRITE_COMBINING` requires PAT programming (`init_pat_wc()`)
  before any page with that flag is mapped. This is done at the start of
  `paging::setup()`, before any identity-map entries are created.
- `KERNEL_VMA_BASE = 0xFFFFFF8000000000` provides the higher-half view
  of the kernel image.
- The `VirtualMemoryManager` capability trait (`services/virt_mem.rs`) is
  intentionally **unimplemented** — `Vmm` is used directly at init. See
  `invariants-23-services.md` (SVC-D001, orphaned/dead trait).
