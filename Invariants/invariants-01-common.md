# Common Crate — Invariants

**Version:** 0.2.0
**Source:** `common/src/types.rs`, `common/src/serial.rs`
**Status:** Stable — shared protocol between bootloader and kernel

---

## State Invariants

**COMMON-001 — `#[repr(C)]` layout compatibility:**
All hand-off types (`MemoryRegion`, `FramebufferInfo`, `PixelFormat`) are
`#[repr(C)]` so the bootloader and kernel (compiled as separate binaries)
agree on memory layout.
- Location: `common/src/types.rs:7,15,40`

**COMMON-002 — MemoryRegionKind covers all UEFI memory types:**
The enum variants map 1:1 to the UEFI memory types that the kernel needs
to distinguish. Unknown UEFI types are classified as `Reserved`.
- Location: `common/src/types.rs:15-26`, `boot/src/main.rs:185-196`

**COMMON-003 — `FramebufferInfo.stride` is pixels, not bytes:**
`Bytes per row = stride * 4`. This matches UEFI GOP semantics.
- Location: `common/src/types.rs:35-36`

**COMMON-004 — Serial `IoBackend` trait provides register-level abstraction:**
The same `SerialPort<B>` code drives both x86 port I/O and RISC-V MMIO UART.
- Location: `common/src/serial.rs:8-11`

---

## Safety Invariants

**COMMON-S001 — Port I/O correctness (x86_64 backend):**
The inline asm `in`/`out` instructions are safe to call at any time because
they only access the 16550 UART at fixed port `0x3F8 + offset`.
- Location: `common/src/serial.rs:107-121`

**COMMON-S002 — MMIO read/write correctness (RISC-V backend):**
The volatile reads/writes to `0x10000000 + offset` are safe to call because
the UART MMIO region is identity-mapped in the page tables.
- Location: `common/src/serial.rs:138-146`

---

## API Contracts

**COMMON-API-001 — `IoBackend::read_reg` / `write_reg`:**
Called with `offset` values 0..=7 (the 16550 register indices). Callers must
ensure the underlying hardware is initialized first.
- Location: `common/src/serial.rs:9-10`

**COMMON-API-002 — `SerialPort<B>::init()`:**
Must be called before any other `SerialPort` method. Configures 115200 8N1
with FIFO enabled.
- Location: `common/src/serial.rs:31-38`

---

## Design Notes

- The `common` crate is `no_std` and compiles for both UEFI/x86_64 and
  bare-metal RISC-V targets, enforced by the workspace `Cargo.toml`
  dependencies.
- `PixelFormat::BltOnly` is refused at boot time (no linear framebuffer).
  See `boot/src/main.rs:234-238`.
