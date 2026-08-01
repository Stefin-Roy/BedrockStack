# AHCI Block Driver — Invariants

**Version:** 0.4.1
**Date:** 2026-08-01
**Source:** `kernel/src/filesystems/blockdriver/{mod,driver,traits,ahci}.rs`
**Status:** Stable (x86_64 only)

---

## State Invariants

**AHCI-001 — AHCI is a registered `StorageDriver`, initialized via `blockdriver::driver::init_all()` during `Kernel::run()`:**
`AhciDriver` (probe: class=0x01, subclass=0x06, prog_if=0x01) is registered
into the `REGISTRY` (`register_all()`); `driver::init_all()` walks PCI,
probes each device, maps the AHCI BAR (NO_CACHE), resets the HBA, and
initializes ports. Returns `Vec<Arc<dyn BlockDevice>>` which is merged with
USB devices and published into the global `BLOCK_DEVICES` registry. The
first block device's first partition is then mounted as `B>` (FAT32) via
`partition::mount_first_partition`.
- Location: `kernel/src/filesystems/blockdriver/driver.rs:20-31,32-72`, `kernel/src/filesystems/blockdriver/ahci.rs:910-928`, `kernel/src/lib.rs:334-348`

**AHCI-002 — MMIO registers are accessed via volatile pointers:**
All MMIO read/write uses `read_volatile`/`write_volatile` to prevent
compiler reordering.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:97-110`

**AHCI-003 — Pre-allocated command table pages for each slot:**
`AHCI_MAX_SLOTS = 32` slots per port, each with a 4K command table
page. PRDT (Physical Region Descriptor Table) entries point directly
to caller buffer physical pages (zero-copy DMA).
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:33,858-887`

**AHCI-004 — Supports both NCQ and non-NCQ command paths:**
NCQ uses FPDMA QUEUED commands (0x60/0x61) via `write_ncq_fis()`.
Non-NCQ uses standard Register H2D FIS via `write_std_fis()` with
28-bit LBA (0xC8/0xCA) or 48-bit LBA (0x25/0x35) commands.
Per-port `ncq` flag selects the path.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:270-308,375-384`

**AHCI-005 — Translation cache avoids repeated 4-level page walks:**
`TRANS_CACHE_SIZE = 64` entries cache virtual-to-physical translations
for DMA buffer pages. The cache lives in `services/dma.rs` (the shared
`KernelDma` allocator); `build_prdt` resolves caller buffers through
`kernel_services().dma.virt_to_phys()`.
- Location: `kernel/src/services/dma.rs:32-59`, `kernel/src/filesystems/blockdriver/ahci.rs:314-318`

**AHCI-006 — Timeout detection uses HLT-based waits via the universal timer:**
`wait_slots()`, `wait_ssts_det()`, and `port_reset()` all wait with
`universal_timer::wait_until_cond` / `sleep_ms` (5 s, 100 ms, 2 ms
deadlines) — the old APIC-counter `ApicTimeout`/`POLL_FALLBACK_LIMIT`
busy-spin is gone. Yielding to the AHCI IRQ / timer keeps device
emulation schedulable under QEMU TCG.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:137-142,199-234,467-523`

**AHCI-007 — Port reset recovery on command failure:**
If a command fails (TFD error or SERR diagnostic), the port is reset
(COMRESET via `SCTL.DET=1`) before retrying.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:467-524,686`

**AHCI-008 — Async completions tracked via `IoCompletions`:**
`IoCompletions { completed: u32, errors: u32 }` — `all_ok()` returns
`true` if `errors == 0 && completed > 0`.
- Location: `kernel/src/filesystems/blockdriver/traits.rs:14-23`

**AHCI-009 — Per-port NCQ flag probed from IDENTIFY data:**
`ncq: bool` on `AhciPort` is set from IDENTIFY word 76, bit 8.
When `ncq == false`, all I/O uses standard non-NCQ FIS.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:579-580`

**AHCI-010 — `write_std_fis()` for non-NCQ Register H2D FIS:**
Writes a standard Register H2D FIS (type 0x27) for non-NCQ commands.
For 28-bit LBA: device register includes LBA[27:24]; commands 0xC8 (read)
and 0xCA (write). For 48-bit LBA: LBA spans bytes 4-6 and 8-10; commands
0x25 (read) and 0x35 (write). The 48-bit path keeps the ATA task-file
obsolete bits set (0xE0) as required by QEMU's non-NCQ DMA-EXT path.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:287-308`

**AHCI-011 — Non-NCQ batch size limited to 1; PxSACT only for NCQ:**
When `ncq == false`, `submit()` limits the batch to a single request
(`reqs.len().min(1)`). `PxSACT` is only written for NCQ commands;
non-NCQ uses `PxCI` alone. `submit_batch()` refuses to overwrite an
outstanding command bit (a retry must not turn into a permanent wait).
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:600,449-463`

**AHCI-012 — The final PRDT entry carries `PRDT_IOC` (bit 31):**
Completion is requested after the last PRDT entry because NCQ completion
is reported by an SDB FIS — without IOC, QEMU's AHCI path can leave the
command outstanding until the timeout.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:92,410-416`

**AHCI-013 — CPU cache is explicitly managed around DMA (`cache_flush_line`):**
`CLFLUSHOPT` (detected once via CPUID.7.EBX bit 23) is used when
available, else `CLFLUSH`. DMA structures (command list, command tables,
received-FIS, scratch) are mapped NO_CACHE by the shared DMA allocator,
so no flush is needed for them; only the caller's data buffers are
managed: write buffers are flushed before submission; read buffers are
invalidated after completion.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:36-50,627-658`

**AHCI-014 — COMRESET saves and restores port registers:**
`port_reset()` snapshots `CLB/CLBU/FB/FBU/IE` before issuing COMRESET
(some hardware clears them) and restores them afterwards, then re-arms
SERR/IS and `CMD_SUD|CMD_POD|CMD_FRE|CMD_ST`.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:480-519`

**AHCI-015 — The received-FIS area is kept distinct from data buffers:**
The HBA writes D2H/SDB FISes to `PxFB` while commands are in flight;
sharing it with IDENTIFY data (or any PRDT target) would corrupt both
DMA streams. Dedicated command-list, received-FIS, and scratch pages.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:858-867`

---

## Safety Invariants

**AHCI-S001 — PRDT DMA safety:**
PRDT entries point to physical addresses of caller buffer pages. The
caller must ensure the buffers remain valid for the duration of the
I/O request. The AHCI controller writes to these physical addresses
via DMA.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:618-625`

**AHCI-S002 — MMIO BAR mapping safety:**
The AHCI BAR (BAR5) is detected as 32-bit or 64-bit MMIO and mapped via
the kernel VMM with `NO_CACHE | READ | WRITE`. The region is sized from
`CAP.NP` (port count), not a fixed 4 KiB.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:743-761`

**AHCI-S003 — `PortPtr` send/sync across the IRQ path:**
`IRQ_PORTS: Mutex<Vec<PortPtr>>` holds raw pointers to live `AhciPort`
objects for `handle_ahci_irq`. Ports are never dropped while registered,
and the list is only mutated during single-threaded init.
- Location: `kernel/src/filesystems/blockdriver/ahci.rs:146-150,181-192,806-808`

---

## API Contracts

**AHCI-API-001 — `blockdriver::driver::init_all(devices)` → `Vec<Arc<dyn BlockDevice>>`:**
Scans the PCI device list, runs the `StorageDriver` registry, resets AHCI
controllers, probes ports, and returns the block devices. DMA comes from
`kernel_services().dma` (the shared `services::dma::KernelDma` allocator).
Must be called after PCI init and VMM activation. On x86_64 only. Caller is
responsible for merging the result into `BLOCK_DEVICES`.

**AHCI-API-002 — `StorageDriver` trait:**
```rust
pub trait StorageDriver: Send + Sync {
    fn name(&self) -> &str;
    fn probe(&self, dev: &PciDevice) -> bool;
    fn init_controller(&self, dev: &PciDevice, dma: &dyn DmaAllocator)
        -> Result<Vec<Arc<dyn BlockDevice>>, &'static str>;
}
```
- Location: `kernel/src/filesystems/blockdriver/driver.rs:10-18`

**AHCI-API-003 — `BlockDevice` trait:**
```rust
pub trait BlockDevice: Send + Sync {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str>;
    fn sector_count(&self) -> u64;
    fn model_string(&self) -> &str;
}
```
- `submit()` takes a batch of `IoRequest`, each with LBA, count, buffer,
  and direction. Returns completion counts.
- `IoBuffer` can be a virtual `Buf(&mut [u8])`, a const `ConstBuf(&[u8])`
  (read paths), or physical `Phys(u64, usize)` for DMA.
- Location: `kernel/src/filesystems/blockdriver/traits.rs:1-29`

---

## Design Notes

- The AHCI driver is x86_64 only (Q35 ICH9 controller at PCI
  00:1f.2). RISC-V platforms use different storage controllers.
- The driver supports both interrupt-driven and polling completion paths.
  Interrupts are registered once per controller (shared PCI INTx#); each port
  has its own PxIE and `irq_completed` flag, and `handle_ahci_irq` iterates
  all active ports to clear PxIS and record completions.
- `IoBuffer::Phys` is used for DMA directly to/from user buffers,
  avoiding a copy through a bounce buffer.
- AHCI devices flow into the block-device registry and are mounted: the
  first block device's first partition is mounted as `B>` (FAT32) during
  `Kernel::run()` (see `invariants-19-fs-partition.md`).
- HLT-based waits (`wait_until_cond`/`sleep_ms` on the universal timer)
  replace all APIC-counter busy-spin timeouts. See
  `invariants-23-services.md` and `invariants-13-platform-x86_64.md`.
