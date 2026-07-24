# Bootloader — Invariants

**Version:** 0.2.0
**Source:** `boot/src/main.rs`, `boot/src/elf.rs`, `boot/src/allocator.rs`
**Status:** Stable

---

## State Invariants

**BOOT-001 — Kernel ELF LOAD segments are copied to reserved physical addresses:**
The physical span `[p_paddr, p_paddr + p_memsz)` for each `PT_LOAD` segment
is reserved via UEFI `allocate_pages(Address, LOADER_DATA)` before copying.
This prevents firmware/boot-services allocations from sitting under the kernel.
- Location: `boot/src/elf.rs:130-178`
- Validates: `p_memsz >= p_filesz`, `e_phentsize >= 56`, segment bounds.

**BOOT-002 — Memory map is built from the FINAL map after `exit_boot_services`:**
The region buffer capacity is pre-allocated (over-provisioned by `* 2 + 256`).
If the final map exceeds capacity, the bootloader HALTS rather than silently
truncating. Only `CONVENTIONAL` memory is `Usable`; all other types are
`Reserved` so the kernel never hands out frames holding live data.
- Location: `boot/src/main.rs:90-162`
- `classify_memory()` at `boot/src/main.rs:185-196`

**BOOT-003 — Framebuffer info transfer buffer contains one valid entry:**
Pre-allocated as `alloc::vec![fb_info]`, leaked via `core::mem::forget`.
- Location: `boot/src/main.rs:94,168`

**BOOT-004 — All transfer buffers use OS_DATA memory type (0x80000001), with LOADER_DATA fallback:**
Custom `#[global_allocator]` backed by `uefi::boot::allocate_pool(OS_DATA, ...)`.
If `OS_DATA` is rejected (some real firmware rejects OEM types), falls back to
`LOADER_DATA`. All `Vec` allocations persist after `exit_boot_services`.
- Location: `boot/src/allocator.rs`, `boot/src/main.rs:21-22`

**BOOT-005 — Kernel stack is 64 KB + 1 guard page per BSP:**
The guard page is the lowest page; its physical address is passed to the
kernel so it can be left unmapped. The stack is page-allocated (not a Vec),
so it is leaked implicitly (no `forget` needed).
- Location: `boot/src/main.rs:102-116`

**BOOT-006 — Heap transfer buffers are leaked before `exit_boot_services`:**
`core::mem::forget(regions_buf)` and `core::mem::forget(fb_buf)` prevent
Rust from calling `dealloc` after UEFI teardown.
- Location: `boot/src/main.rs:167-168`

**BOOT-007 — RSDP is discovered from UEFI config table before boot services end:**
Config table entries are invalid after `exit_boot_services`.
- Location: `boot/src/main.rs:119,200-214`

**BOOT-008 — `GOP BltOnly` returns zeroed `FramebufferInfo` (not fatal):**
If the framebuffer has no linear address, the bootloader logs a warning and
returns `FramebufferInfo::zeroed()` so the kernel can fall back to serial-only
operation. Never panics.
- Location: `boot/src/main.rs:234-238`

**BOOT-009 — ELF loading rejects PIE executables (`ET_DYN`):**
Only `ET_EXEC` (type 2) is accepted. Accepting `ET_DYN` would silently jump
to a wrong entry point.
- Location: `boot/src/elf.rs:105-107`

**BOOT-010 — Multiboot2/GRUB entry via `kernelmb2` feature:**
A second boot path exists via Multiboot2 (GRUB). An assembly trampoline in
`multiboot2_header.s` enters from 32-bit protected mode, identity-maps 1 GiB
via 2 MiB pages, enters long mode, then calls `rust_entry_mb2()`.
The Rust entry parses the Multiboot2 information structure (memory map,
framebuffer tag, ACPI RSDP tags) and constructs a `Kernel` instance.
Gated behind `#[cfg(feature = "kernelmb2")]`.
- Location: `kernel/src/arch/x86_64/multiboot2_header.s`, `kernel/src/arch/x86_64/multiboot2.rs`

**BOOT-011 — RSDP can be passed as embedded data from Multiboot2 tags:**
In addition to `rsdp_addr` (physical address from UEFI config table), the
RSDP can be passed as `rsdp_data: Option<&'static [u8]>` containing the
embedded RSDP bytes from Multiboot2 ACPI tags (types 14 and 15). This avoids
needing to map from a physical address before VMM activation.
- Location: `kernel/src/arch/x86_64/multiboot2.rs`, `kernel/src/lib.rs`, `common/src/types.rs`

**BOOT-012 — CLI + CLD executed before kernel entry:**
The bootloader executes `CLI` (disable interrupts) before `MOV RSP` to
prevent firmware IDT from handling an interrupt on the kernel's unprotected
stack. `CLD` guarantees the SysV ABI direction-flag invariant before any
`cld`-dependent string operation in the kernel.
- Location: `boot/src/main.rs`

**BOOT-013 — `FramebufferInfo::zeroed()` provides safe default:**
When GOP is unavailable (missing handle, open failure, or BltOnly), a
zeroed `FramebufferInfo` (address=0, bpp=0) is returned. The kernel's
display subsystem treats a null framebuffer pointer as a safe no-op.
- Location: `common/src/types.rs`

## Safety Invariants

**BOOT-S001 — `elf::load_elf` safety:**
Caller must ensure `elf_data` points to a valid ELF64 binary and that target
physical memory is writable and does not overlap critical regions.
- Location: `boot/src/elf.rs:77-80`

**BOOT-S002 — `jump_to_kernel` safety:**
`entry` must be a valid kernel entry point. `stack_top` must be valid
writable memory (stack grows downward). `regions_ptr` / `fb_ptr` must point
to valid, non-dangling data. This function does not return.
- Location: `boot/src/main.rs:260-268`

**BOOT-S003 — Kernel entry ABI (x86_64):**
sysv64 calling convention: `rdi=regions_ptr, rsi=regions_len, rdx=fb_ptr,
rcx=stack_guard, r8=rsdp_addr`. `rsdp_addr` is 0 if RSDP not found.
For the Multiboot2 path, the `Kernel::new()` call additionally receives
`rsdp_data: Option<&'static [u8]>` in lieu of `rsdp_addr`.
- Location: `boot/src/main.rs:281-292`, `kernel/src/arch/x86_64/multiboot2.rs`

---

## API Contracts

**BOOT-API-001 — `elf::load_elf` input:**
Validates ELF magic, class (64-bit), encoding (little-endian), machine type
matching target arch. Program headers must have `phentsize >= 56`.
- Location: `boot/src/elf.rs:82-128`

**BOOT-API-002 — Transfer buffer capacity:**
`regions_buf` capacity is over-provisioned as `est_entries * 2 + 256`.
The bootloader HALTS if capacity is exceeded (post-exit allocation is
impossible).
- Location: `boot/src/main.rs:90-93,150-155`

---

## Design Notes

- `OS_DATA` memory type (`0x80000001`) is a custom UEFI type that persists
  after `exit_boot_services(Some(OS_DATA))`.
- On x86_64, conventional memory ABOVE 4 GiB is only filtered under
  hypervisors (detected via CPUID hypervisor bit). Real hardware trusts the
  UEFI memory map and keeps its full RAM.
- The kernel ELF is stored on the ESP as `\EFI\BEDROCK\KERNEL` and loaded
  from disk at runtime (not embedded at build time).
- When booting via GRUB/Multiboot2, the kernel is loaded by GRUB directly
  from the ESP as a multiboot2 ELF module (no separate bootloader executable).
