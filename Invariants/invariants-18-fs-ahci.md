# AHCI Block Driver — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/filesystems/blockdriver/{mod,traits,ahci}.rs`
**Status:** Stable (x86_64 only)

---

## State Invariants

**AHCI-001 — AHCI is initialized once during `Kernel::run()`:**
Scans PCI bus for the AHCI controller (Q35 ICH9), maps its BAR using
the kernel VMM (with NO_CACHE), performs controller reset, enables
ports, and allocates command tables.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs` (init flow)

**AHCI-002 — MMIO registers are accessed via volatile pointers:**
All MMIO read/write uses `read_volatile`/`write_volatile` to prevent
compiler reordering.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:75-80`

**AHCI-003 — Pre-allocated command table pages for each slot:**
`AHCI_MAX_SLOTS = 32` slots per port, each with a 4K command table
page. PRDT (Physical Region Descriptor Table) entries point directly
to caller buffer physical pages (zero-copy DMA).
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:26-27`

**AHCI-004 — Supports both NCQ and non-NCQ command paths:**
NCQ uses FPDMA QUEUED commands (0x60/0x61) via `write_ncq_fis()`.
Non-NCQ uses standard Register H2D FIS via `write_std_fis()` with
28-bit LBA (0xC8/0xCA) or 48-bit LBA (0x25/0x35) commands.
Per-port `ncq` flag selects the path.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:6-7`

**AHCI-005 — Translation cache avoids repeated 4-level page walks:**
`TRANS_CACHE_SIZE = 64` entries cache virtual-to-physical translations
for DMA buffer pages.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:28`

**AHCI-006 — Timeout detection via APIC timer count:`
The APIC timer count is read before a command and compared with the
current count to detect stalled commands.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:23`

**AHCI-007 — Port reset recovery on command failure:`
If a command fails (TFD error or SERR diagnostic), the port is reset
before retrying.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:11`

**AHCI-008 — Async completions tracked via `IoCompletions`:`
`IoCompletions { completed: u32, errors: u32 }` — `all_ok()` returns
`true` if `errors == 0 && completed > 0`.
- Location: `kernel/src/filesystems/blockdriver/traits.rs:13-22`

**AHCI-009 — Per-port NCQ flag probed from IDENTIFY data:**
`ncq: bool` on `AhciPort` is set from IDENTIFY word 76, bit 8.
When `ncq == false`, all I/O uses standard non-NCQ FIS.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs`

**AHCI-010 — `write_std_fis()` for non-NCQ Register H2D FIS:**
Writes a standard Register H2D FIS (type 0x27) for non-NCQ commands.
For 28-bit LBA: device register includes LBA[27:24]; commands 0xC8 (read)
and 0xCA (write). For 48-bit LBA: LBA spans bytes 4-6 and 8-10; commands
0x25 (read) and 0x35 (write).
- Location: `kernel/src/filesystems/blockdriver/ahci.rs`

**AHCI-011 — Non-NCQ batch size limited to 1; PxSACT only for NCQ:**
When `ncq == false`, `submit()` limits the batch to a single request
(`reqs.len().min(1)`). `PxSACT` is only written for NCQ commands;
non-NCQ uses `PxCI` alone.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs`

## Safety Invariants

**AHCI-S001 — PRDT DMA safety:**
PRDT entries point to physical addresses of caller buffer pages. The
caller must ensure the buffers remain valid for the duration of the
I/O request. The AHCI controller writes to these physical addresses
via DMA.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:12`

**AHCI-S002 — MMIO BAR mapping safety:**
The AHCI BAR is detected as 32-bit or 64-bit MMIO and mapped via the
kernel VMM with `NO_CACHE | READ | WRITE`. The mapped virtual address
must be below `PCI_VADDR_FLOOR`.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:15-16`

---

## API Contracts

**AHCI-API-001 — `ahci::init(page_table_root, phys_allocator)`:**
Scans PCI, finds AHCI controller, resets it, probes ports, and
registers discovered devices with the VFS block device layer.
Must be called after PCI init and VMM activation.

**AHCI-API-002 — `BlockDevice` trait:**
```rust
pub trait BlockDevice: Send + Sync {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str>;
    fn sector_count(&self) -> u64;
    fn model_string(&self) -> &str;
}
```
- `submit()` takes a batch of `IoRequest`, each with LBA, count, buffer,
  and direction. Returns completion counts.
- `IoBuffer` can be either a virtual `Buf(&mut [u8])` or physical
  `Phys(u64, usize)` for DMA.
- Location: `kernel/src/filesystems/blockdriver/traits.rs:24-28`

---

## Design Notes

- The AHCI driver is x86_64 only (Q35 ICH9 controller at PCI
  00:1f.2). RISC-V platforms use different storage controllers.
- The driver operates in polling mode (no interrupts).
- `IoBuffer::Phys` is used for DMA directly to/from user buffers,
  avoiding a copy through a bounce buffer.
- AHCI is currently configured but NOT connected to the VFS mount
  logic (no filesystem driver reads from AHCI yet).
