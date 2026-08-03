# Common Crate — Invariants

**Version:** 0.3.0
**Source:** `common/src/types.rs`
**Status:** Stable — shared protocol between bootloader and kernel

---

## State Invariants

**COMMON-001 — `#[repr(C)]` layout compatibility:**
All hand-off types (`MemoryRegion`, `FramebufferInfo`, `PixelFormat`) are
`#[repr(C)]` so the bootloader and kernel (compiled as separate binaries)
agree on memory layout.
- Location: `common/src/types.rs:7,29,93`

**COMMON-002 — MemoryRegionKind covers all UEFI memory types:**
The enum variants map 1:1 to the UEFI memory types that the kernel needs
to distinguish. Unknown UEFI types are classified as `Reserved`.
- Location: `common/src/types.rs:15-26`, `boot/src/main.rs:239-250`

**COMMON-003 — `FramebufferInfo.stride` is pixels, not bytes:**
`Bytes per row = stride * bpp`. This matches UEFI GOP semantics.
- Location: `common/src/types.rs:35`

---

## Safety Invariants

**COMMON-S001 — Port I/O correctness (x86_64 backend):**
Previously documented at `common/src/serial.rs`. The `IoBackend` abstraction
now lives in `common/src/serial/` with per-architecture backends.
The inline asm `in`/`out` instructions are safe to call at any time because
they only access the 16550 UART at fixed port `0x3F8 + offset`.

**COMMON-S002 — MMIO read/write correctness (RISC-V backend):**
The volatile reads/writes to `0x10000000 + offset` are safe to call because
the UART MMIO region is identity-mapped in the page tables.

---

## API Contracts

**COMMON-API-001 — `IoBackend::read_reg` / `write_reg`:**
Called with `offset` values 0..=7 (the 16550 register indices). Callers must
ensure the underlying hardware is initialized first.

**COMMON-API-002 — `SerialPort::init()`:**
Must be called before any other `SerialPort` method. Configures 115200 8N1
with FIFO enabled.

---

## Design Notes

- The `common` crate is `no_std` and compiles for both UEFI/x86_64 and
  bare-metal RISC-V targets, enforced by the workspace `Cargo.toml`
  dependencies.
- `PixelFormat::BltOnly` is refused at boot time (no linear framebuffer).
  See `boot/src/main.rs:368-371`.
