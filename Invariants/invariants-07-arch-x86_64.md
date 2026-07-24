# x86_64 Architecture — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/arch/x86_64/{gdt,idt,paging,trampoline,serial,mod}.rs`
**Status:** Stable

---

## State Invariants

**PAGING-001 — Page tables are freshly built (not firmware's):**
Temporary UEFI page tables are replaced by our own PML4, avoiding
firmware huge-page mappings that would silently block our flags.
- Location: `kernel/src/arch/x86_64/paging.rs`

**PAGING-002 — Identity mapping covers `[0, ram_end)`, framebuffer extension beyond:**
`ram_end = allocator.alloc_end().max(apic_base + PAGE_4K)` rounded to 2 MiB.
No hardcoded 4 GiB minimum. Bulk RAM uses 2 MiB huge pages; kernel image,
NULL page's 2 MiB chunk, and guard page's chunk use 4 KiB pages.
If the framebuffer sits above `ram_end`, a separate identity-map extension
covers `[fb_map_start, fb_map_end)` with `WRITE_COMBINING` caching.
- Location: `kernel/src/arch/x86_64/paging.rs:36-81`

**PAGING-003 — W^X policy:**
No page is both writable and executable. `.text` = READ + EXECUTE, `.rodata`
= READ + NX, everything else = WRITE + NX. Requires EFER.NXE and CR0.WP,
both enabled in `setup()`.
- Location: `kernel/src/arch/x86_64/paging.rs:41-44,97-111`

**PAGING-004 — NULL page (frame 0) is unmapped:**
The first 4 KiB of the first 2 MiB chunk is skipped during mapping.
Null dereferences fault instead of corrupting memory.
- Location: `kernel/src/arch/x86_64/paging.rs:61-63`

**PAGING-005 — Stack guard page is unmapped:**
The guard page's physical address (passed from bootloader) is skipped
during identity mapping. Stack overflow hits the unmapped page and
faults to the double-fault handler (via IST).
- Location: `kernel/src/arch/x86_64/paging.rs:47,61-63`

**PAGING-006 — Trampoline code at `0x8000` is executable:**
The page at physical address `0x8000` is mapped with EXECUTE permission.
- Location: `kernel/src/arch/x86_64/paging.rs:66-68`

**PAGING-007 — PAT entry 1 programmed as Write-Combining (01h):**
`init_pat_wc()` writes `IA32_PAT` MSR (0x277) so that PAT entry 1
(bits 15:8) is `01h` (WC). Called at the top of `paging::setup()`,
before any identity-map entries are created.
- Location: `kernel/src/mm/vmm/x86_64.rs`, `kernel/src/arch/x86_64/paging.rs`

**PAGING-008 — Framebuffer identity map uses WRITE_COMBINING (not NO_CACHE):**
Identity-map pages that overlap the bootloader framebuffer region are
mapped with `PageFlags::WRITE_COMBINING` instead of `PageFlags::NO_CACHE`.
This enables the CPU to coalesce flush stores into burst writes over the bus.
APIC and other MMIO regions remain mapped as `NO_CACHE`.
- Location: `kernel/src/arch/x86_64/paging.rs`

**PAGING-009 — Local APIC MMIO is identity-mapped as NO_CACHE:**
The `IA32_APIC_BASE` MSR is read during `paging::setup()` and the local
APIC 4 KiB page is mapped as `NO_CACHE` in the identity page tables.
This ensures the APIC registers are accessible before `Arch::init()`.
- Location: `kernel/src/arch/x86_64/paging.rs`

---

### GDT

**GDT-001 — TSS provides IST entry for double-fault:**
`DOUBLE_FAULT_IST_INDEX = 0` points into per-CPU double-fault stacks.
The double-fault handler uses this IST so stack-overflow → triple-fault
cannot occur.
- Location: `kernel/src/arch/x86_64/gdt.rs:17`

**GDT-002 — Per-CPU double-fault stacks are pre-allocated:**
`DF_STACKS: [[u8; 5*4096]; 16]` — enough for `MAX_CPUS = 16` CPUs.
Each CPU's TSS.IST[0] points into its own slot, preventing cross-CPU
stack corruption on simultaneous double faults.
- Location: `kernel/src/arch/x86_64/gdt.rs:20,23-24,57-63`

**GDT-003 — GDT and TSS are pinned in static memory:**
`CPU_GDT` and `CPU_TSS` are `static mut` arrays. The GDT descriptor
encodes their address, so they must not move after init.
- Location: `kernel/src/arch/x86_64/gdt.rs:29,34`

---

### IDT

**IDT-001 — IDT is initialized once via `spin::Once`:**
The BSP builds the IDT in `IDT.call_once(|| ...)`. APs reload it via
`init_ap()` which panics if the BSP hasn't initialized it.
- Location: `kernel/src/arch/x86_64/idt.rs:8,36,140-146`

**IDT-002 — Double-fault handler uses IST stack:**
`set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX)` marks the double-fault
descriptor to switch stacks on entry.
- Location: `kernel/src/arch/x86_64/idt.rs:53-57`

**IDT-003 — APIC timer at vector 32 with interrupt gate:`
`.disable_interrupts(true)` — clears IF so no nested interrupts.
- Location: `kernel/src/arch/x86_64/idt.rs:60`

**IDT-004 — All exception handlers (except breakpoint) log and halt:`
Delegates to `kerneldump::dump_full_fault()` with vector and error code.
- Location: `kernel/src/arch/x86_64/idt.rs:87-137`

---

### Trampoline

**TRAMP-001 — AP trampoline is copied to `0x8000` (physical):**
16-bit real-mode code that transitions through protected mode to
long mode. CR3 is loaded before paging is enabled (on INIT+SIPI the
AP's CR3 is 0 from reset).
- Location: `kernel/src/arch/x86_64/trampoline.rs:8,23-137`

**TRAMP-002 — `TrampolineData` at `0x8700` is `#[repr(C)]`:**
Contains cr3, stack_top, entry, per_cpu_ptr, started_flag_addr,
lm_entry (long-mode entry). Written by BSP, read by AP.
- Location: `kernel/src/arch/x86_64/trampoline.rs:13-21`

---

## Safety Invariants

**PAGING-S001 — `paging::setup` safety:**
`allocator` must be initialised with free frames. The returned `Vmm`
must be activated before any address translation depends on it.
- Location: `kernel/src/arch/x86_64/paging.rs:22-24`

**GDT-S001 — GDT/TSS manipulation safety:**
`static mut` accesses to `DF_STACKS`, `CPU_TSS`, `CPU_GDT` are safe
because each CPU writes only its own slot (indexed by CPU ID) during
single-threaded init before SMP starts.
- Location: `kernel/src/arch/x86_64/gdt.rs:53,66-67,75-86`

**IDT-S001 — IDT `init` safety:**
Must be called AFTER GDT init (the double-fault IST stack requires a
valid TSS in the GDT).
- Location: `kernel/src/arch/x86_64/idt.rs:33-37`

---

## API Contracts

**GDT-API-001 — `gdt::init()`:**
Must be called once per CPU (BSP then each AP). Loads segments and TSS.
Called from `Arch::init()` (BSP) and `Arch::init_ap()` (APs).

**IDT-API-001 — `idt::init()`:**
Must be called after `gdt::init()`. Loads the IDT via `lidt`.
Called once on BSP; APs call `idt::init_ap()`.

---

## Design Notes

- The GDT has four entries: null (implicit), kernel code (0x08), kernel
  data (0x10), and TSS (0x18). The TSS descriptor also has entries at
  0x20 for the IST stack selector management.
- NXE is enabled AFTER setting LME=1 (CR0.PG = 1 with LME=1 → LMA=1).
  Writing NXE before LMA=1 would #GP on some CPUs.
- The AP trampoline loads CR3 twice: once in 32-bit mode (truncated to
  32 bits from memory at `[0x8700]`), then again in 64-bit mode with
  the full 64-bit value. This is intentional because the 32-bit `mov eax, [mem]`
  instruction cannot hold a 64-bit address.
