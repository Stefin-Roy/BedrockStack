# BedrockOS - Invariants v0.1

**Version**: 0.2.0
**Date**: 2026-07-11
**Status**: ACPI subsystem — RSDP discovery, table parsing, AML, reset/shutdown

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

**ALLOC-04**: The kernel heap is backed by physical frames from the bitmap allocator.
- Location: `kernel/src/mm/heap.rs`
- `heap::init()` allocates 64 initial pages (256 KB) via `BitmapAllocator::alloc()`.
- When the heap runs out, `GlobalAlloc::alloc()` grows by allocating 16 more pages.

**ALLOC-05**: The heap free list is singly-linked through free block headers.
- Location: `kernel/src/mm/heap.rs`
- Each free block starts with a `BlockHeader` containing `size` and `next` pointer.
- Allocated blocks return a payload address immediately after the header.
- Adjacent free blocks are coalesced during `push_free()`.

**ALLOC-06**: The heap allocator is protected by `spin::Mutex` for interrupt safety.
- Location: `kernel/src/mm/heap.rs`
- The `#[global_allocator]` static wraps `Mutex<HeapInner>`.
- Interrupt handlers calling `alloc`/`dealloc` will spin-wait if the main thread holds the lock.

### Page Table State

**PAGING-01**: Paging uses a freshly built PML4 (not the firmware's tables),
loaded via a single `Cr3::write`.
- Location: `kernel/src/arch/x86_64/paging.rs`
- Building our own table avoids UEFI huge-page mappings silently blocking our
  flags (e.g. framebuffer `NO_CACHE`).

**PAGING-02**: Identity mapping covers all physical memory from 0 to
max(4GB, framebuffer_end, allocator.alloc_end()).
- Location: `kernel/src/arch/x86_64/paging.rs`
- Bulk RAM uses 2 MiB huge pages (fast, compact). The kernel image, the NULL
  page's 2 MiB chunk, and the guard page's chunk use 4 KiB pages.
- Covers kernel code/data, framebuffer, all managed RAM (incl. the stack and
  hand-off buffers), and hardware-mapped regions within the range.

**PAGING-03**: W^X — no page is both writable and executable.
- Location: `kernel/src/arch/x86_64/paging.rs` (`leaf_flags_4k`)
- `.text` = executable + read-only; `.rodata` = read-only + NX; everything else
  = writable + NX. Requires EFER.NXE and CR0.WP, both enabled in `setup`.

**PAGING-04**: The NULL page (frame 0) and the stack guard page are unmapped, so
null derefs and stack overflows fault instead of corrupting memory.
- Location: `kernel/src/arch/x86_64/paging.rs`

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

### ACPI State

**ACPI-01**: ACPI tables are parsed from the RSDP during `Kernel::init()`, after
page tables are built and the higher-half VMM is activated.
- Location: `kernel/src/lib.rs` (`Kernel::init_acpi`)
- The RSDP physical address is discovered by the bootloader from the UEFI config table
  (`ACPI2_GUID`) and passed to the kernel in register `r10` (x86_64) or `a4` (RISC-V).
- On RISC-V when booted by OpenSBI, the RSDP is read from the DTB `chosen` node
  `acpi-rsdp` property.

**ACPI-02**: `AcpiSubsystem` is stored as `Option<AcpiSubsystem>` in `Kernel`. If
RSDP discovery or table parsing fails, the kernel continues without ACPI.
- Location: `kernel/src/lib.rs` (`Kernel.acpi`)
- `reset()` and `shutdown()` are no-ops if `self.acpi` is `None`.

**ACPI-03**: The AML interpreter is initialised after page tables are live but before
interrupts are enabled.
- Location: `kernel/src/lib.rs` (`Kernel::init`)
- Requires the heap allocator (AML uses `alloc`).
- `init_aml()` is called via `AcpiSubsystem::init_aml()` which also executes
  `\_INI` and `\_SB._INI` AML methods automatically.

**ACPI-04**: System reset uses the FADT `reset_reg` register if the FADT flags
indicate support (bit 10 of `FixedFeatureFlags`). Falls back to the 8042 PS/2
keyboard controller method on x86. Ultimate fallback is an infinite halt.
- Location: `kernel/src/acpi/mod.rs` (`AcpiSubsystem::reset`)

**ACPI-05**: System shutdown (S5 soft-off) writes SLP_TYP + SLP_EN to the
PM1 control registers. The SLP_TYP value for S5 is obtained from evaluating
`\_S5` in the AML namespace, falling back to `0x00` if AML is unavailable.
- Location: `kernel/src/acpi/mod.rs` (`AcpiSubsystem::shutdown`, `s5_slp_typ`)
- Second fallback on x86: direct write to the PM1a_CNT IO port with
  `SLP_TYP=0, SLP_EN=1` (works on QEMU ICH9/PIIX4).

**ACPI-06**: The VMM-backed `AcpiHandler` uses the active page table to map physical
regions inside a reserved virtual address range (`ACPI_VADDR_BASE`, 256 MB below
`KERNEL_VMA_BASE`).  A bump allocator advances `next_vaddr` downward; mappings are
never unmapped.  IO-port access delegates to x86 `in`/`out` instructions (RISC-V
returns 0).
- Location: `kernel/src/acpi/mod.rs` (`AcpiHandler`, `AcpiVmmState`, `init_vmm`)

**ACPI-07**: The ACPI VMM state (`ACPI_STATE`) holds a raw pointer to the kernel's
`BitmapAllocator`.  It is always accessed behind a `Mutex`.  The raw pointer is
valid for the kernel's lifetime because the allocator lives in `Kernel`.
- Location: `kernel/src/acpi/mod.rs` (`AcpiVmmState.alloc`, `map_physical_region`)

### APIC / Timer State

**APIC-01**: Local APIC is enabled before the timer is programmed.
- Location: `kernel/src/arch/x86_64/apic.rs` (`init`)
- Bit 11 of `IA32_APIC_BASE` MSR is set.
- Spurious Interrupt Vector Register (offset `0xF0`) has bit 8 set.

**APIC-02**: The APIC timer is calibrated via PIT channel 0 before starting.
- Location: `kernel/src/arch/x86_64/apic.rs` (`calibrate_via_pit`)
- PIT is programmed in one-shot mode with count 0xFFFF (~54.9 ms at 1.193182 MHz).
- The APIC timer runs simultaneously with maximum initial count.
- The elapsed APIC ticks during the PIT period are used to compute the count for
  100 Hz (10 ms period).
- Formula: `count = elapsed_ticks * 1193182 / 6553500`.

**APIC-03**: The APIC timer interrupt is delivered at vector 32.
- Location: `kernel/src/arch/x86_64/idt.rs`
- LVT Timer register (offset `0x320`) is configured with periodic mode (bit 17)
  and vector 32.
- The timer handler writes the EOI register (offset `0xB0`) via `apic::apic_eoi()`.

**APIC-04**: The APIC MMIO region is within the identity-mapped first 4 GiB.
- Location: `kernel/src/arch/x86_64/paging.rs` (`setup`)
- The LAPIC base (typically `0xFEE00000`) falls within `min_end = 4 GiB`.
- No dedicated `NO_CACHE` flag is applied to the APIC range (left as
  `WRITABLE | NO_EXECUTE`; MTRRs handle uncacheability on real hardware).

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

**INIT-04**: Physical allocator must exist before heap init.
- Location: `kernel/src/lib.rs` (`Kernel::new`: bitmap allocator created, then `heap::init` called)

**INIT-05**: Heap allocator must exist before any `alloc`-based code runs.
- Location: `kernel/src/lib.rs` (`Kernel::new` returns; modules in `run()` use `alloc`)

**INIT-06**: APIC must be initialised after IDT (timer handler registered in IDT).
- Location: `kernel/src/arch/x86_64/mod.rs` (`init`: `gdt::init` → `idt::init` → `apic::init`)

**INIT-07**: Interrupts must be enabled after APIC init and page table setup.
- Location: `kernel/src/lib.rs` (`Kernel::init`: APIC in `CurrentArch::init`, then page tables,
  then ACPI init, then `CurrentArch::enable_interrupts`)

**INIT-08**: Page tables must be set up before framebuffer use.
- Location: `kernel/src/lib.rs` (`Kernel::init` runs `setup_virt_mem` before `run`)

**INIT-09**: ACPI RSDP must be discovered from the UEFI configuration table before
`exit_boot_services`, because the config table entries are invalid after boot services end.
- Location: `boot/src/main.rs` (`find_rsdp`)

**INIT-10**: UEFI boot services must be exited before bare metal code runs.
- Location: `boot/src/main.rs` (`exit_boot_services`)

**INIT-11**: Kernel ELF must be loaded into physical memory before exit_boot_services.
- Location: `boot/src/main.rs` (`elf::load_elf`)

**INIT-12**: Transfer buffers and the kernel stack must be allocated before
exit_boot_services.
- Location: `boot/src/main.rs`

**INIT-13**: ACPI tables must be parsed after the higher-half page tables are
activated (the VMM-backed `AcpiHandler` requires live page tables with a
reserved virtual address range).
- Location: `kernel/src/lib.rs` (`Kernel::init`:
  `CurrentArch::init` → `switch_to_higher_half` → `init_acpi` → `enable_interrupts`)

---

## 3. API Contracts

**API-01**: `Module::init()` must return `Ok(())` or `Err(&'static str)`.
- Location: `kernel/src/module/mod.rs`

**API-02**: `Driver::shutdown()` must be safe to call multiple times.
- Location: `kernel/src/drivers/traits.rs`

**API-03**: `Module::name()` must return a valid UTF-8 string.
- Location: `kernel/src/module/mod.rs`

**API-04**: Kernel `_start` (x86_64) receives (memory_map_ptr, memory_map_len,
framebuffer_ptr, stack_guard, rsdp_addr).
- Location: `kernel/src/main.rs`
- sysv64 ABI: rdi=memory_map_ptr, rsi=memory_map_len, rdx=framebuffer_ptr,
  rcx=stack_guard, r10=rsdp_addr. Callers must provide valid, non-null pointers.
  `rsdp_addr` is 0 if the RSDP was not found.

**API-05**: RISC-V `rust_entry` receives (hart_id, dtb_ptr) with `rsdp_addr`
discovered internally from the DTB or QEMU virt fallback.

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
- Location: `kernel/src/arch/x86_64/paging.rs`

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

**NOTE-07**: Boot crate uses sysv64 calling convention for kernel entry (x86_64).
- Location: `boot/src/main.rs`
- rdi=regions_ptr, rsi=regions_len, rdx=fb_ptr, rcx=stack_guard, r10=rsdp_addr.

**NOTE-08**: Custom allocator in boot/src/allocator.rs uses OS_DATA memory type.
- All `Vec` allocations in boot crate use this allocator.
- Memory persists after `exit_boot_services(Some(OS_DATA))`.

**NOTE-09**: IDT registers handlers for divide-error, breakpoint, invalid-opcode,
invalid-TSS, segment-not-present, stack-segment-fault, general-protection-fault,
page-fault, double-fault, and APIC timer (vector 32). The double-fault handler
uses the GDT/TSS IST stack. The timer handler calls `apic::apic_eoi()`. All
exception handlers (except breakpoint) log the fault and halt.
- Location: `kernel/src/arch/x86_64/idt.rs`

**NOTE-10**: Kernel heap allocator is a linked-list free-list allocator.
- 64 pages (256 KB) initial pool; grows by 16 pages per exhaustion.
- Protected by `spin::Mutex`. Physical pages obtained from `BitmapAllocator`.
- All physical RAM is identity-mapped, so heap pages are accessed at their
  physical addresses (virtual == physical).
- Location: `kernel/src/mm/heap.rs`

**NOTE-11**: APIC timer uses raw MMIO access to LAPIC registers.
- The LAPIC base address is read from `IA32_APIC_BASE` MSR (0x1B).
- Calibration uses PIT channel 0 in one-shot mode; raw `in`/`out` port I/O.
- Formula: `count = elapsed_ticks * 1193182 / 6553500` for 10 ms period.
- Falls back to a hard-coded value of 1,000,000 (QEMU default 100 MHz) if PIT
  calibration times out or yields zero.
- Location: `kernel/src/arch/x86_64/apic.rs`

**NOTE-12**: ACPI is an independent subsystem, not part of the `Arch` trait.
The `acpi` crate v6.1.1 provides ACPI table parsing and AML interpretation.
The `AcpiHandler` uses the VMM to map physical regions inside a reserved range
below `KERNEL_VMA_BASE` and delegates port I/O to x86 `in`/`out` instructions.
The `aml` feature flag enables the built-in AML interpreter (no separate `aml`
crate).
- Location: `kernel/src/acpi/mod.rs`, `kernel/Cargo.toml`

**NOTE-13**: The `Arch` trait in `kernel/src/arch/mod.rs` abstracts architecture
differences. `CurrentArch` resolves to `X86_64` or `Riscv64` based on
`cfg(target_arch = ...)`. The x86_64 `Arch` impl calls:
  `gdt::init()` → `idt::init()` → `apic::init()`.
- All arch-specific modules live under `kernel/src/arch/<arch>/`.

---

## 7. Update Checklist

When modifying code:

- [ ] ALLOC-01 through ALLOC-06 still hold
- [ ] PAGING-01 through PAGING-04 still hold (incl. W^X and NULL/guard holes)
- [ ] APIC-01 through APIC-04 still hold
- [ ] MOD-01 and MOD-02 still hold
- [ ] DISP-01 still holds (pixel format propagated correctly)
- [ ] BOOT-01 through BOOT-06 still hold
- [ ] INIT-01 through INIT-13 ordering maintained
- [ ] ACPI-01 through ACPI-07 still hold
- [ ] No new unsafe blocks without safety comment
- [ ] No new panics in non-test code
- [ ] If boot types changed, update both boot/src/main.rs AND kernel/src/boot.rs
- [ ] Heap changes: verify free-list integrity, coalesce logic, `Send`/`Sync` impls
