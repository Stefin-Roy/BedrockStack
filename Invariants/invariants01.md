# BedrockOS - Invariants v0.1

**Version**: 0.1.2
**Date**: 2026-07-11
**Status**: Post-audit fix (huge-page paging, W^X, guard page, load-region reservation)

---

## 1. State Invariants

Properties of system state that must always hold.

### Memory Allocator State

**ALLOC-01**: Each frame is either allocated or free, never both.
- Location: `kernel/src/mm/phys_alloc.rs`
- Each bit in the bitmap represents one frame. 1 = allocated, 0 = free.

**ALLOC-02**: The bitmap covers exactly `(total_frames + 7) / 8` bytes.
- Location: `kernel/src/mm/phys_alloc.rs` (`bitmap_len`)

**ALLOC-03**: Reserved frames (from UEFI memory map) are marked allocated at initialization.
- Location: `kernel/src/mm/phys_alloc.rs:52-64`

### Page Table State

**PAGING-01**: Paging uses a freshly built PML4 (not the firmware's tables),
loaded via a single `Cr3::write`.
- Location: `kernel/src/mm/virt_mem.rs`
- Building our own table avoids UEFI huge-page mappings silently blocking our
  flags (e.g. framebuffer `NO_CACHE`).

**PAGING-02**: Identity mapping covers all physical memory from 0 to
max(4GB, framebuffer_end, allocator.managed_end()).
- Location: `kernel/src/mm/virt_mem.rs`
- Bulk RAM uses 2 MiB huge pages (fast, compact). The kernel image, the NULL
  page's 2 MiB chunk, and the guard page's chunk use 4 KiB pages.
- Covers kernel code/data, framebuffer, all managed RAM (incl. the stack and
  hand-off buffers), and hardware-mapped regions within the range.

**PAGING-03**: W^X — no page is both writable and executable.
- Location: `kernel/src/mm/virt_mem.rs` (`leaf_flags_4k`)
- `.text` = executable + read-only; `.rodata` = read-only + NX; everything else
  = writable + NX. Requires EFER.NXE and CR0.WP, both enabled in `setup`.

**PAGING-04**: The NULL page (frame 0) and the stack guard page are unmapped, so
null derefs and stack overflows fault instead of corrupting memory.
- Location: `kernel/src/mm/virt_mem.rs`

### Module State

**MOD-01**: Each module in MODULES is initialized at most once.
- Location: `kernel/src/module/registry.rs`

**MOD-02**: If a module fails to initialize, subsequent modules are not initialized.
- Location: `kernel/src/module/registry.rs:35-38`

### Display State

**DISP-01**: Framebuffer pixel format is respected when drawing.
- Location: `kernel/src/display/framebuffer.rs`
- `Framebuffer::new()` stores the pixel format from `FramebufferInfo`.
- `draw_char()` writes pixel bytes in the correct order (BGR or RGB).

### Boot Loader State

**BOOT-01**: Kernel ELF LOAD segments are copied to their specified physical
addresses, into a range first reserved from UEFI via `allocate_pages(Address)`.
- Location: `boot/src/elf.rs`
- Reserving the load span (as LOADER_DATA) prevents firmware/boot-services
  allocations from sitting under the kernel and being clobbered by the copy.
- Validates (overflow-safe): e_phentsize >= 56, p_memsz >= p_filesz, segment
  data within bounds. Copies via `copy_nonoverlapping`.

**BOOT-02**: Memory map transfer buffer is built from the FINAL map returned by
`exit_boot_services` (not a stale pre-allocation snapshot), into a buffer whose
capacity was reserved beforehand (no allocation after exit).
- Location: `boot/src/main.rs`
- Capacity is over-provisioned; if the final map would exceed it, the boot
  loader HALTS with a serial error rather than silently truncating the map.
- Only `CONVENTIONAL` memory is reported `Usable`; OS_DATA (stack + hand-off
  buffers), loader/boot-services, ACPI and unknown/MMIO types are `Reserved`, so
  the kernel never hands out frames holding its own live data.

**BOOT-03**: Framebuffer info transfer buffer contains one valid FramebufferInfo.
- Location: `boot/src/main.rs`

**BOOT-04**: Transfer buffers use OS_DATA allocator (MemoryType 0x80000001).
- Location: `boot/src/allocator.rs`
- Custom `#[global_allocator]` backed by `uefi::boot::allocate_pool(OS_DATA, ...)`.
- All `Vec` allocations go through this allocator and persist after exit_boot_services.

**BOOT-05**: Kernel stack is 64 KB, page-allocated from OS_DATA memory with one
extra guard page at the bottom. The guard page's physical address is passed to
the kernel (rcx) so the kernel leaves it unmapped.
- Location: `boot/src/main.rs`

**BOOT-06**: Heap transfer buffers (regions, framebuffer info) are leaked (not
dropped) before exit_boot_services. The stack is page-allocated (not a Vec) so
it needs no `forget`.
- Location: `boot/src/main.rs`
- `core::mem::forget()` prevents Rust from calling `dealloc` after UEFI teardown.

---

## 2. Boot Sequence Dependencies

**INIT-01**: GDT must be loaded before IDT.
- Location: `kernel/src/arch/x86_64/mod.rs` (`init`), called from `kernel/src/lib.rs` (`Kernel::init`)

**INIT-02**: IDT must be loaded before any interrupt is enabled.
- Location: `kernel/src/arch/x86_64/mod.rs` (`init`)

**INIT-03**: Physical allocator must exist before page table setup.
- Location: `kernel/src/lib.rs` (`Kernel::new` then `Kernel::init`)

**INIT-04**: Page tables must be set up before framebuffer use.
- Location: `kernel/src/lib.rs` (`Kernel::init` runs `virt_mem::setup` before `run`)

**INIT-05**: UEFI boot services must be exited before bare metal code runs.
- Location: `boot/src/main.rs` (`exit_boot_services`)

**INIT-06**: Kernel ELF must be loaded into physical memory before exit_boot_services.
- Location: `boot/src/main.rs` (`elf::load_elf`)

**INIT-07**: Transfer buffers and the kernel stack must be allocated before
exit_boot_services.
- Location: `boot/src/main.rs`

---

## 3. API Contracts

**API-01**: `Module::init()` must return `Ok(())` or `Err(&'static str)`.
- Location: `kernel/src/module/mod.rs`

**API-02**: `Driver::shutdown()` must be safe to call multiple times.
- Location: `kernel/src/drivers/traits.rs`

**API-03**: `Module::name()` must return a valid UTF-8 string.
- Location: `kernel/src/module/mod.rs`

**API-04**: Kernel `_start` receives (memory_map_ptr, memory_map_len,
framebuffer_ptr, stack_guard).
- Location: `kernel/src/main.rs`
- sysv64 ABI: rdi=memory_map_ptr, rsi=memory_map_len, rdx=framebuffer_ptr,
  rcx=stack_guard. Callers must provide valid, non-null pointers.

---

## 4. Programming Disciplines

**RUST-01**: No use-after-free of physical frames.
- Enforced by: Rust ownership system.

**RUST-02**: No data races on framebuffer.
- Enforced by: `Framebuffer` is `!Sync`. Single-threaded kernel.

**RUST-03**: No null pointer dereference in framebuffer.
- Enforced by: `Framebuffer::new()` asserts address is non-zero and 4-byte aligned.

**RUST-04**: No buffer overflow in bitmap allocator.
- Enforced by: Bitmap size is checked at initialization.

---

## 5. Derived Properties

**DERIVED-01**: No double allocation follows from ALLOC-01.

**DERIVED-02**: No double-free is prevented by ALLOC-01.

**DERIVED-03**: Kernel receives valid memory map follows from BOOT-02 and INIT-07.

---

## 6. Implementation Notes

**NOTE-01**: Font is 128 entries, 16 bytes each.
- Location: `kernel/src/display/framebuffer.rs`

**NOTE-02**: Bitmap allocator uses linear scan.
- Location: `kernel/src/mm/phys_alloc.rs`

**NOTE-03**: Page table setup uses x86_64 crate's `OffsetPageTable` with
phys_offset=0 over a freshly allocated PML4. Identity mapping: virtual == phys.
Bulk RAM is mapped with 2 MiB huge pages; the kernel image / NULL / guard chunks
use 4 KiB pages for per-section W^X and to punch holes.
- Location: `kernel/src/mm/virt_mem.rs`

**NOTE-04**: GDT has null (implicit), code, data, and TSS segments. The TSS
provides an IST entry (index 0) used by the double-fault handler so it always
runs on a known-good stack (prevents stack-overflow -> triple fault).
- Location: `kernel/src/arch/x86_64/gdt.rs`

**NOTE-05**: Boot types (MemoryRegion, FramebufferInfo, etc.) and the COM1
serial driver live in the shared `common` crate and are used by BOTH the boot
and kernel crates. `common` is `no_std` and compiles for both targets, so there
is a single source of truth (no duplication / sync hazard). `#[repr(C)]` ensures
layout compatibility across the two separately-compiled binaries.

**NOTE-06**: The kernel ELF is copied to the ESP as `\EFI\BEDROCK\KERNEL` and
loaded from disk at runtime by the bootloader (not embedded at build time).
- Location: `create_image.py`, `boot/src/main.rs`

**NOTE-07**: Boot crate uses sysv64 calling convention for kernel entry.
- Location: `boot/src/main.rs`
- rdi=regions_ptr, rsi=regions_len, rdx=fb_ptr, rcx=stack_guard.

**NOTE-08**: Custom allocator in boot/src/allocator.rs uses OS_DATA memory type.
- All `Vec` allocations in boot crate use this allocator.
- Memory persists after `exit_boot_services(Some(OS_DATA))`.

**NOTE-09**: IDT registers handlers for divide-error, breakpoint, invalid-opcode,
invalid-TSS, segment-not-present, stack-segment-fault, general-protection-fault,
page-fault, and double-fault. The double-fault handler uses the GDT/TSS IST
stack. All handlers (except breakpoint) log the fault and halt.
- Location: `kernel/src/arch/x86_64/idt.rs`

---

## 7. Update Checklist

When modifying code:

- [ ] ALLOC-01 through ALLOC-03 still hold
- [ ] PAGING-01 through PAGING-04 still hold (incl. W^X and NULL/guard holes)
- [ ] MOD-01 and MOD-02 still hold
- [ ] DISP-01 still holds (pixel format propagated correctly)
- [ ] BOOT-01 through BOOT-06 still hold
- [ ] INIT-01 through INIT-07 ordering maintained
- [ ] No new unsafe blocks without safety comment
- [ ] No new panics in non-test code
- [ ] If boot types changed, update both boot/src/main.rs AND kernel/src/boot.rs
