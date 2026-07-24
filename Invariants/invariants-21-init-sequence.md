# Boot Initialization Sequence — Invariants

**Version:** 0.2.0
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
  │  └── Memory map from EBS         │    │  ├── Enter long mode      │
  └────────────────┬─────────────────┘    │  └── rust_entry_mb2()     │
                   │                      └───────────┬───────────────┘
                   └────────────┬──────────────────────┘
                                ▼
                    Kernel::new()
                    ├── BitmapAllocator::new()
                    ├── Framebuffer::new() (detects bpp from GOP/GRUB tag)
                    ├── heap::init()
                    │
                    ▼
                    Kernel::init()
                    ├── heap::set_phys_allocator()
                    ├── smp::early_init_bsp()
                    ├── switch_to_higher_half()
                    │   └── Arch::setup_virt_mem()
                    │       (builds identity + higher-half page tables)
                    ├── Vmm::activate()
                    │   (switches CR3 / SATP)
                    ├── enable_framebuffer_log()
                    │   (Console init moved here — after page tables
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
                    ├── smp::init()
                    │   ├── Arch::discover_cpus()
                    │   ├── Allocate AP stacks
                    │   └── Arch::wake_aps()
                    ├── Arch::enable_interrupts()
                    │
                    ▼
                    Kernel::run()
                    ├── PCI::init()
                    ├── AHCI::init() [x86_64 only]
                    ├── VFS::init()
                    │   ├── fstypes::register_all()
                    │   ├── Mount tmpfs on A>
                    │   └── Create tmp/dev
                    ├── module::init_all()
                    └── Halt loop
```

---

## Ordering Invariants

**INIT-001 — GDT must be loaded before IDT:**
The double-fault handler's IST entry must be valid in the TSS (part of
GDT) before the IDT can reference it.
- Location: `kernel/src/arch/x86_64/mod.rs:init()`

**INIT-002 — IDT must be loaded before interrupts are enabled:**
The IDT must be valid before the CPU can take any interrupt or exception.
- Location: `kernel/src/arch/x86_64/mod.rs:init()`

**INIT-003 — Physical allocator must exist before page table setup:**
Page-table intermediate frames are allocated from `BitmapAllocator`.
- Location: `kernel/src/lib.rs:` `Kernel::new` → `Kernel::init`

**INIT-003b — Physical allocator must be re-pointed at start of `init()`:**
`heap::set_phys_allocator(&mut self.allocator)` is called at the top of
`init()` (before any heap activity) so that the heap can grow through
the correct `PHYS_ALLOCATOR` pointer. This prevents stale-pointer
corruption during `log::info!`, string formatting, or Vec allocations.
- Location: `kernel/src/lib.rs:` `init()` → `set_phys_allocator()`

**INIT-004 — Physical allocator must exist before heap init:**
Heap pages are allocated from `BitmapAllocator`.
- Location: `kernel/src/lib.rs:` `Kernel::new` calls `heap::init(&mut allocator)`

**INIT-005 — Heap must exist before any `alloc`-based code:**
All code after `heap::init()` may use `Vec`, `Box`, `Arc`, etc.
- Location: `kernel/src/lib.rs:` `Kernel::new` returns; `init()` and `run()` use `alloc`

**INIT-006 — APIC must be initialized after IDT:**
Timer handler registered in IDT before APIC timer is programmed.
- Location: `kernel/src/arch/x86_64/mod.rs:init()`

**INIT-007 — Page tables must be set up before ACPI init:**
The VMM-backed `AcpiHandler` requires live page tables for MMIO mapping.
- Location: `kernel/src/lib.rs:` `switch_to_higher_half()` → `init_acpi()`

**INIT-008 — ACPI must be parsed before I/O APIC init:**
I/O APIC base addresses and GSI mappings come from the MADT table.
- Location: `kernel/src/lib.rs:` `init_acpi()` → `init_ioapic()`

**INIT-009 — I/O APIC must be initialized before SMP AP startup:**
APs may generate interrupts that the I/O APIC must route.
- Location: `kernel/src/lib.rs:` `init_ioapic()` → `smp::init()`

**INIT-010 — Page tables must be set up before framebuffer console init:**
Framebuffer memory must be identity-mapped before `enable_framebuffer_log()`
creates a `Console` that draws to the framebuffer. Console init was moved
from `Kernel::new()` to `Kernel::init()` after `switch_to_higher_half()` to
ensure page tables cover the framebuffer physical address.
- Location: `kernel/src/lib.rs:` `switch_to_higher_half()` then `enable_framebuffer_log()`

**INIT-011 — Interrupts must be enabled after SMP init:**
AP startup uses IPIs (x86_64) or SBI ecalls (RISC-V). Interrupts are
enabled only after all CPUs are running.
- Location: `kernel/src/lib.rs:` `smp::init()` → `enable_interrupts()`

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

**INIT-016 — VFS init must happen after PCI and AHCI init:**
VFS mounts block devices discovered by PCI/AHCI.
- Location: `kernel/src/lib.rs:` PCI → AHCI → VFS

**INIT-017 — Module init runs last:**
Modules may use VFS, display, and all other initialized subsystems.
- Location: `kernel/src/lib.rs:` `init_all()` at end of `run()`

---

## Design Notes

- The sequence is **strictly serial**: no concurrency until SMP is up.
- AP init runs parallel on multiple CPUs AFTER `smp::init()` returns,
  but the BSP does not enable interrupts until after that.
- ACPI AML interpreter (commented out in current code) would be called
  between ACPI init and I/O APIC init, requiring a valid heap.
