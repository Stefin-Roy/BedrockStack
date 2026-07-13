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

**BOOT-004 — All transfer buffers use OS_DATA memory type (0x80000001):**
Custom `#[global_allocator]` backed by `uefi::boot::allocate_pool(OS_DATA, ...)`.
All `Vec` allocations persist after `exit_boot_services`.
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

**BOOT-008 — `GOP BltOnly` is fatal at boot:**
If the framebuffer has no linear address, the bootloader panics rather than
handing the kernel an invalid pointer.
- Location: `boot/src/main.rs:234-238`

**BOOT-009 — ELF loading rejects PIE executables (`ET_DYN`):**
Only `ET_EXEC` (type 2) is accepted. Accepting `ET_DYN` would silently jump
to a wrong entry point.
- Location: `boot/src/elf.rs:105-107`

---

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
- Location: `boot/src/main.rs:281-292`

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
- On x86_64, x86_64 conventional memory ABOVE 4 GiB is filtered out because
  OVMF/QEMU may report it without real RAM backing.
- The kernel ELF is stored on the ESP as `\EFI\BEDROCK\KERNEL` and loaded
  from disk at runtime (not embedded at build time).
