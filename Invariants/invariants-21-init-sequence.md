# Boot Initialization Sequence — Invariants

**Version:** 0.6.0
**Date:** 2026-08-01
**Source:** `kernel/src/lib.rs`, `kernel/src/main.rs`, `boot/src/main.rs`
**Status:** Stable

---

## Initialization Order (Directed Graph)

The following dependencies MUST be respected:

```
  ┌─ UEFI boot path ─────────────────┐    ┌─ GRUB/Multiboot2 path ────┐
  │  bootloader (UEFI)               │    │  GRUB loads kernel ELF    │
  │  ├── ELF loaded to phys mem      │    │  ├── 32-bit asm entry     │
  │  ├── RSDP from config table      │    │  ├── Identity-map 1 GiB   │
  │  ├── Memory map from EBS         │    │  ├── Enter long mode      │
  │  ├── cpu_slow MSRs (Intel-only)  │    │  └── rust_entry_mb2()     │
  │  └── jump_to_kernel(CLI+CLD)     │    └───────────┬───────────────┘
  └────────────────┬─────────────────┘                │
                   └────────────┬──────────────────────┘
                                ▼
                    Kernel::new()
                    ├── acpi_log::init()
                    ├── find_bitmap_region()
                    ├── KernelLayout (sections from linker symbols)
                    ├── BitmapAllocator::new()
                    ├── Reserve kernel image region
                    ├── Framebuffer::new() (with shadow buffer)
                    ├── heap::init()
                    │
                    ▼
                    Kernel::init()
                    ├── heap::set_phys_allocator()
                    ├── smp::early_init_bsp()
                    ├── switch_to_higher_half()
                    │   └── CurrentArch::setup_virt_mem()
                    │       (identity + higher-half, NXE+WP, PAT WC)
                    │   └── Vmm::activate()
                    │       (switches CR3 / SATP; acpi::init_vmm also here)
                    ├── enable_framebuffer_log()
                    │   (Console via set_console() — after page tables
                    │    cover framebuffer physical address)
                    ├── CurrentArch::init()
                    │   ├── GDT::init()
                    │   ├── IDT::init()
                    │   └── APIC::init()
                    │       └── PIT calibration
                    ├── ACPI::init_vmm()
                    ├── AcpiSubsystem::new()
                    │   (parses RSDP data or mapped RSDP)
                    ├── IOAPIC::init() [x86_64 only]
                    ├── services::init_services(root, alloc, acpi)
                    │   └── Box::leak → KernelServices global
                    ├── smp::init(page_table_root, acpi, services)
                    │   ├── services.cpu.discover_cpus(acpi)
                    │   ├── Allocate AP stacks (alloc_contiguous)
                    │   ├── Configure PerCpu slots
                    │   └── services.cpu.wake_aps()
                    ├── services.platform.enable_interrupts()
                    │
                    ▼
                    Kernel::run()
                    ├── heap::set_phys_allocator() (re-point after move)
                    ├── IDT protect (.idt section read-only) [x86_64]
                    ├── PCI::init() (ECAM mapping + bus enumeration)
                    ├── input::init() (UInputL core — static queue)
                    ├── ps2::init() [x86_64] (8042 keyboard, IRQ 1/GSI 1;
                    │   registers "PS/2 Keyboard" device with UInputL)
                    ├── blockdriver::driver::init_all() [x86_64]
                    │   ├── AHCI + block_devices returned
                    │   └── DMA via kernel_services().dma (shared KernelDma)
                    ├── USB/xHCI::init_all() [x86_64]
                    │   └── DMA via kernel_services().dma (shared KernelDma)
                    ├── VFS::init()
                    │   ├── fstypes::register_all()
                    │   ├── Mount tmpfs on A>
                    │   └── Mount ESP fat32 on B> [x86_64]
                    ├── module::init_all()
                    │   ├── HelloModule
                    │   ├── Fat32Test (B>)
                    │   ├── MsixTest [x86_64]
                    │   ├── UsbTest [x86_64]
                    │   ├── Fat32Ls [B>]
                    │   ├── VfsTest (A>)
                    │   └── InputTest (UInputL echo, Backspace/Delete,
                    │       halts on Esc) [x86_64]
                    └── Halt loop (reached only when no input device / non-x86_64)
```

---

## Ordering Invariants

**INIT-001 — GDT must be loaded before IDT:**
The double-fault handler's IST entry must be valid in the TSS (part of
GDT) before the IDT can reference it.
- Location: `kernel/src/arch/x86_64/mod.rs` `X86_64::init()`

**INIT-002 — IDT must be loaded before interrupts are enabled:**
The IDT must be valid before the CPU can take any interrupt or exception.
- Location: `kernel/src/arch/x86_64/mod.rs` `X86_64::init()`

**INIT-003 — Physical allocator must exist before page table setup:**
Page-table intermediate frames are allocated from `BitmapAllocator`.
- Location: `kernel/src/lib.rs:` `Kernel::new` → `Kernel::init`

**INIT-003b — Physical allocator must be re-pointed at start of `init()`:**
`heap::set_phys_allocator(&mut self.allocator)` is called at the top of
`init()` (before any heap activity) so that the heap can grow through
the correct `PHYS_ALLOCATOR` pointer. This prevents stale-pointer
corruption during `log::info!`, string formatting, or Vec allocations.
Also re-called at the start of `Kernel::run()` after the final move.
- Location: `kernel/src/lib.rs:` `init()` → `set_phys_allocator()`, `run()` → `set_phys_allocator()`

**INIT-004 — Physical allocator must exist before heap init:**
Heap pages are allocated from `BitmapAllocator`.
- Location: `kernel/src/lib.rs:` `Kernel::new` calls `heap::init(&mut allocator)`

**INIT-005 — Heap must exist before any `alloc`-based code:**
All code after `heap::init()` may use `Vec`, `Box`, `Arc`, etc.
- Location: `kernel/src/lib.rs:` `Kernel::new` returns; `init()` and `run()` use `alloc`

**INIT-006 — APIC must be initialized after IDT:**
Timer handler registered in IDT before APIC timer is programmed.
- Location: `kernel/src/arch/x86_64/mod.rs` `X86_64::init()`

**INIT-007 — Page tables must be set up before ACPI init:**
The VMM-backed `AcpiHandler` requires live page tables for MMIO mapping.
- Location: `kernel/src/lib.rs:` `switch_to_higher_half()` → `init_acpi()`

**INIT-008 — ACPI must be parsed before I/O APIC init:**
I/O APIC base addresses and GSI mappings come from the MADT table.
- Location: `kernel/src/lib.rs:` `init_acpi()` → `init_ioapic()`

**INIT-009 — I/O APIC must be initialized before SMP AP startup:**
APs may generate interrupts that the I/O APIC must route.
- Location: `kernel/src/lib.rs:` `init_ioapic()` → `smp::init()`

**INIT-009b — Services must be built after ACPI/IOAPIC, before SMP:**
`init_services()` wraps the `AcpiSubsystem` (Box::leak) and installs the
capability providers; `smp::init` and all later subsystems consume
`KernelServices` via `kernel_services()`.
- Location: `kernel/src/lib.rs:199-207`, `kernel/src/services/mod.rs:62-72`

**INIT-010 — Page tables must be set up before framebuffer console init:**
Framebuffer memory must be identity-mapped before `enable_framebuffer_log()`
calls `set_console()` with a `Console` that draws to the framebuffer.
Console init moved from `Kernel::new()` to after `switch_to_higher_half()`.
- Location: `kernel/src/lib.rs:` `switch_to_higher_half()` then `enable_framebuffer_log()`

**INIT-011 — Interrupts must be enabled after SMP init:**
AP startup uses IPIs (x86_64) or SBI ecalls (RISC-V). Interrupts are
enabled only after all CPUs are running, via `services.platform`.
- Location: `kernel/src/lib.rs:` `smp::init()` → `platform.enable_interrupts()`

**INIT-011b — `acpi_log::init()` runs first in `Kernel::new()`:**
Before any allocator or heap initialization, the ACPI log subsystem is
initialized to capture early boot messages.
- Location: `kernel/src/lib.rs:88-89`

**INIT-011c — Framebuffer shadow buffer allocated from `BitmapAllocator`:**
During `Kernel::new()`, a contiguous shadow buffer is allocated from the
physical allocator (size = `stride * height * bpp`). This provides a
cacheable drawing surface before page tables map the real framebuffer.
- Location: `kernel/src/lib.rs:124-132`

**INIT-012 — RSDP discovery must happen before `exit_boot_services`:**
UEFI config table entries are invalid after boot services end.
- Location: `boot/src/main.rs:` `find_rsdp()` before `exit_boot_services()`

**INIT-013 — UEFI boot services must be exited before kernel entry:**
After `exit_boot_services()`, only runtime services remain. Any UEFI
protocol call would fault.
- Location: `boot/src/main.rs:` `exit_boot_services()` → `jump_to_kernel()`

**INIT-014 — Kernel ELF must be loaded before boot services exit:**
The `allocate_pages(Address)` reservation requires boot services.
- Location: `boot/src/main.rs:` `elf::load_elf()` before `exit_boot_services()`

**INIT-015 — Transfer buffers and stack must be allocated before EBS:**
All `Vec` allocations use OS_DATA allocator; `forget` prevents dealloc
after exit.
- Location: `boot/src/main.rs:` buffer allocation before `exit_boot_services()`

**INIT-016 — PCI init must happen before AHCI and xHCI init:**
AHCI and xHCI device discovery scans the PCI device list built by `pci::init()`.
- Location: `kernel/src/lib.rs:` PCI → AHCI → xHCI

**INIT-016b — IDT lock-down happens at start of `Kernel::run()`:**
The `.idt` section is made read-only after all IDT initialization is
complete, preventing wild writes from corrupting the interrupt table.
- Location: `kernel/src/lib.rs:284-289`

**INIT-016c — VFS init happens after block driver init:**
VFS mounts block devices discovered by AHCI. The ESP (first partition on
first block device) is mounted as `B>` (fat32) after VFS init.
- Location: `kernel/src/lib.rs:` block drivers → VFS → partition mount

**INIT-017 — Module init runs last:**
Modules may use VFS, display, input, and all other initialized subsystems.
The x86_64 module list includes: `HelloModule`, `Fat32Test`, `MsixTest`,
`UsbTest`, `Fat32Ls`, `VfsTest`, `InputTest`. Non-x86_64 targets exclude
`MsixTest`, `UsbTest`, and `InputTest`. `InputTest` is last and halts forever
once an input device is present, so it is the terminal step of the sequence.
- Location: `kernel/src/lib.rs:` `init_all()` at end of `run()`

**INIT-017a — UInputL core init runs before any input driver:**
`input::init()` (the core owns the static event queue and registries) must
precede `ps2::init()`, which registers a device and submits events. The queue
is a `static const` so no heap ordering is involved, but the call still marks
the ordering contract for future drivers.
- Location: `kernel/src/lib.rs:323-325`, `kernel/src/input/mod.rs:96-101`

**INIT-017b — PS/2 init runs after PCI init and UInputL init, before module tests:**
`ps2::init()` registers the keyboard ISR and programs IOAPIC GSI 1 while
interrupts are already enabled (post-`init()`). It must complete before
`module::init_all()` so `InputTest` can receive keystrokes. Failure is
non-fatal — the keyboard simply stays absent and `InputTest` skips itself
(no `register_device` call, so `input::device_count()` stays 0).
- Location: `kernel/src/lib.rs:327-331`, `kernel/src/drivers/ps2.rs:768-784`

---

## Design Notes

- The sequence is **strictly serial**: no concurrency until SMP is up.
- AP init runs parallel on multiple CPUs AFTER `smp::init()` returns,
  but the BSP does not enable interrupts until after that.
- ACPI AML interpreter (commented out in current code) would be called
  between ACPI init and I/O APIC init, requiring a valid heap.
