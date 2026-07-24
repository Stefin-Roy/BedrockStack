# BedrockOS Invariants — Index

**Version:** 0.3.0
**Date:** 2026-07-24
**Status:** All subsystems documented — MM, Arch (x86_64, RISC-V), ACPI, Display, PCI, Platform,
Module, Drivers, VFS, Tmpfs, AHCI, SMP, Kerneldump, Boot, Common, Partition

---

## Scope

This document set captures the **invariants** — properties of system state,
unsafe code preconditions, API contracts, and design rules — that must hold
for BedrockOS to be correct. Each file covers one subsystem, matching the
Rust module hierarchy under `kernel/src/`.

---

## File Index

| #  | File | Subsystem | Source Paths |
|----|------|-----------|-------------|
| 00 | `invariants-00-INDEX.md` | Index & conventions | — |
| 01 | `invariants-01-common.md` | Shared types | `common/src/types.rs` |
| 02 | `invariants-02-boot.md` | Bootloader | `boot/src/main.rs`, `boot/src/elf.rs`, `boot/src/allocator.rs` |
| 03 | `invariants-03-mm-physalloc.md` | Physical allocator | `kernel/src/mm/phys_alloc.rs` |
| 04 | `invariants-04-mm-heap.md` | Heap allocator | `kernel/src/mm/heap.rs` |
| 05 | `invariants-05-mm-vmm.md` | Virtual memory manager | `kernel/src/mm/vmm/mod.rs` + `vmm/{x86_64,riscv64}.rs` |
| 06 | `invariants-06-arch.md` | Arch trait | `kernel/src/arch/mod.rs` |
| 07 | `invariants-07-arch-x86_64.md` | x86_64 arch | `kernel/src/arch/x86_64/{gdt,idt,paging,trampoline,serial}.rs` |
| 08 | `invariants-08-arch-riscv64.md` | RISC-V64 arch | `kernel/src/arch/riscv64/{mod,paging,trap,sbi,trampoline}.rs` |
| 09 | `invariants-09-acpi.md` | ACPI subsystem | `kernel/src/acpi/{mod,tables,madt,mcfg,fadt,gas,platform,interrupt}.rs` |
| 10 | `invariants-10-display.md` | Display / framebuffer (legacy) | `kernel/src/display/{mod,framebuffer}.rs` |
| 10g | `invariants-10-graphics-framebuffer.md` | Graphics framebuffer (active) | `graphics/Framebuffer/src/{display,framebuffer,console,color}.rs` |
| 11 | `invariants-11-module.md` | Module system | `kernel/src/module/{mod,registry,vfs_test}.rs` |
| 12 | `invariants-12-pci.md` | PCI subsystem | `kernel/src/pci/{mod,ecam,enumerate}.rs` |
| 13 | `invariants-13-platform-x86_64.md` | x86_64 platform | `kernel/src/platform/x86_64_pc/{apic,ioapic,pit}.rs` |
| 14 | `invariants-14-platform-riscv64.md` | RISC-V platform | `kernel/src/platform/riscv_virt/{plic,clint,htif}.rs` |
| 15 | `invariants-15-drivers-serial.md` | Serial driver | `kernel/src/drivers/serial.rs` |
| 16 | `invariants-16-fs-vfs.md` | VFS core | `kernel/src/filesystems/vfs/{mod,dentry,inode,superblock,file,fdtable,mount,drive,path,irq,types}.rs` |
| 17 | `invariants-17-fs-tmpfs.md` | tmpfs | `kernel/src/filesystems/fstypes/{mod,tmpfs}.rs` |
| 18 | `invariants-18-fs-ahci.md` | AHCI block driver | `kernel/src/filesystems/blockdriver/{mod,traits,ahci}.rs` |
| 19 | `invariants-19-fs-partition.md` | Partition tables (MBR/GPT) + FAT32 BPB | `kernel/src/filesystems/partition/{mod,mbr,gpt}.rs`, `kernel/src/filesystems/fstypes/fat32.rs` |
| 19s | `invariants-19-smp.md` | SMP | `kernel/src/smp/mod.rs` |
| 20 | `invariants-20-kerneldump.md` | Fault dump | `kernel/src/kerneldump/{mod,dump,disasm}.rs` |
| 21 | `invariants-21-init-sequence.md` | Boot ordering | `kernel/src/lib.rs`, `kernel/src/main.rs` |
| 22 | `invariants-22-derived.md` | Derived properties | All |

---

## Naming Convention

Invariant IDs follow the pattern `AREA-NNN` where:

- `AREA` is a short subsystem code: `ALLOC`, `HEAP`, `VMM`, `PAGING`, `BOOT`,
  `ACPI`, `DISP`, `MOD`, `PCI`, `APIC`, `IOAPIC`, `PIT`, `SMP`, `VFS`, `TMPFS`,
  `AHCI`, `SERIAL`, `PLAT`, `ARCH`, `DUMP`, `INIT`, `COMMON`, `PART`, `WC`
- `NNN` is a three-digit number

Example: `ALLOC-001`, `PAGING-003`, `ACPI-007`.

---

## Cross-Reference Guide

| Invariant area | Also affected by |
|---|---|
| Physical allocator (`ALLOC`) | Page tables (`PAGING`), heap (`HEAP`), ACPI VMM, PCI VMM |
| Page tables (`PAGING`) | VMM, boot loader memory map, WC (PAT), APIC mapping |
| WC / Write-Combining (`WC`) | Framebuffer (`DISP`), PAT MSR (`PAGING`), `PageFlags` (`VMM`) |
| Heap (`HEAP`) | All kernel code running after `heap::init()` |
| Serial driver (`SERIAL`) | All logging output, must not deadlock |
| VFS (`VFS`) | Module init, tmpfs, AHCI |
| SMP (`SMP`) | Per-CPU data, serial prefix, AP startup |

---

## Update Checklist

When modifying code, verify that relevant invariants still hold:

- [ ] **ALLOC**: bitmap integrity, no double-alloc, no reserved-frame alloc
- [ ] **HEAP**: free-list integrity, coalesce logic, `Send`/`Sync` impls
- [ ] **VMM**: page alignment, no double-map, no leak of page-table frames
- [ ] **PAGING**: W^X, NULL/guard unmapped, identity coverage, higher-half alias, PAT WC programming, APIC NO_CACHE
- [ ] **WC**: PAT MSR setup order, WRITE_COMBINING page flag propagation, framebuffer WC vs APIC NO_CACHE
- [ ] **BOOT**: ELF load validation, memory-map classification, OS_DATA hand-off
- [ ] **ACPI**: table-checksum validation, VMM state, fallback correctness
- [ ] **DISP**: pixel format propagation, bounds checks, font-table immutability, shadow buffer flush correctness, dirty-rect tracking
- [ ] **APIC/IOAPIC/PIT**: interrupt delivery, timer calibration, EOI ordering
- [ ] **SMP**: PerCpu layout, AP startup sequence, stack-guard unmapping
- [ ] **SERIAL**: lock ordering, per-CPU re-entrancy, no deadlock
- [ ] **VFS**: IrqMutex discipline, dentry/inode lifetime, dcache consistency
- [ ] **TMPFS**: atomic counter, per-inode locking, no deadlock
- [ ] **AHCI**: DMA safety, MMIO ordering, PRDT bounds, timeout handling, NCQ vs non-NCQ FIS selection
- [ ] **FAT**: BPB discriminant validation (RootEntCnt, FATSz16, FATSz32), per-field bounds checks
- [ ] **PCI**: ECAM VMM, read/write alignment, device enumeration
- [ ] **KERNELDUMP**: re-entrancy guard, NMI safety
- [ ] If boot types changed, update both `boot/src/main.rs` AND `common/src/types.rs`
- [ ] If Multiboot2 entry changed, update both `multiboot2_header.s` AND `multiboot2.rs`
- [ ] If framebuffer address/size changed, verify shadow-buffer allocation matches scanout buffer dimensions
- [ ] If arch-specific trampoline changed, update both BSP and AP entry paths
