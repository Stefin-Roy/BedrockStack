# Partition Table Parsing — Invariants

**Version:** 0.1.0
**Source:** `kernel/src/filesystems/partition/{mod,mbr,gpt}.rs`
**Status:** Stable

---

## State Invariants

**PART-001 — Both MBR and GPT detection share a common entry point:**
`probe()` reads LBA 0, checks the 0x55AA signature, then inspects
partition entry 0 for type 0xEE (protective MBR) to decide whether to
parse as GPT or MBR.
- Location: `kernel/src/filesystems/partition/mod.rs:98-120`

**PART-002 — MBR supports extended partitions via EBR chain traversal:**
When a primary partition entry has type 0x05 or 0x0F (extended), the
parser follows the EBR chain starting at the extended partition's base
LBA. Each EBR yields one logical partition (entry 0) and optionally
points to the next EBR (entry 1).
- Location: `kernel/src/filesystems/partition/mbr.rs:52-98`

**PART-003 — EBR traversal is bounded:**
`MAX_EBR_CHAIN = 100` limits the depth of the EBR chain to prevent
infinite loops from corrupted or malicious partition tables.
- Location: `kernel/src/filesystems/partition/mod.rs:17`

**PART-004 — GPT header CRC32 is validated:**
The GPT header CRC32 field is verified by zeroing the crc32 field in
the header buffer and recomputing the CRC over `header_size` bytes.
- Location: `kernel/src/filesystems/partition/gpt.rs:48-54`

**PART-005 — Partition numbering follows platform conventions:**
MBR primary partitions use slots 1–4 (matching their entry index).
Logical partitions start at 5 and increment sequentially. GPT
partitions number sequentially from 1 in entry order.
- Location: `kernel/src/filesystems/partition/mbr.rs:39`

**PART-006 — `PartitionDevice` adjusts LBAs transparently:**
Every `IoRequest` submitted through a `PartitionDevice` has its LBA
offset by the partition's `start_lba`. The `sector_count()` returns
the partition's sector count, not the whole disk's.
- Location: `kernel/src/filesystems/partition/mod.rs:50-91`

---

## Safety Invariants

**PART-S001 — Packed struct field access via raw pointers:**
GPT partition entries are `#[repr(C, packed)]` and their `name` field
(`[u16; 36]`) may be misaligned. It is read via `ptr::read_unaligned`
through `addr_of!` to avoid creating a misaligned reference.
- Location: `kernel/src/filesystems/partition/gpt.rs:89`

**PART-S002 — MBR entry access via raw pointer cast:**
MBR partition entries at offsets 0x1BE–0x1ED within the sector buffer
are accessed via `*const MbrEntry` raw pointer cast. The `MbrEntry`
struct is `#[repr(C, packed)]` so field reads are implicitly
unaligned-safe (by-value copies, not references).
- Location: `kernel/src/filesystems/partition/mbr.rs:23,30`

**PART-S003 — IoBuffer reborrow in PartitionDevice:**
`PartitionDevice::submit()` creates temporary `IoBuffer::Buf`
references from the original request buffers by deriving a raw pointer
and re-dereferencing it. The temporary references outlive neither the
call nor the original borrow, preventing aliasing violations.
- Location: `kernel/src/filesystems/partition/mod.rs:67-74`

---

## API Contracts

**PART-API-001 — `probe(device: Arc<dyn BlockDevice>) -> Result<PartitionTable, &'static str>`:**
Auto-detects MBR vs GPT. Returns `PartitionTable::Mbr(Vec<PartitionInfo>)`
or `PartitionTable::Gpt(Vec<PartitionInfo>)`. Fails if no valid
signature is found or I/O errors occur.

**PART-API-002 — `mount_partition(device, part_number, fstype, drive) -> Result<(), VfsError>`:**
Probes the device, finds partition `part_number`, wraps it in a
`PartitionDevice`, and mounts `fstype` on `drive`. Returns
`VfsError::NotFound` if the partition does not exist or is extended.

**PART-API-003 — `mount_first_partition(device, fstype, drive) -> Result<(), VfsError>`:**
Same as `mount_partition` but selects the first non-extended partition
regardless of its number.

**PART-API-004 — `PartitionDevice` implements `BlockDevice`:**
```rust
pub struct PartitionDevice {
    inner: Arc<dyn BlockDevice>,
    start_lba: u64,
    sector_count: u64,
    model: String,
}
```
- `submit()` delegates to `inner` after adding `start_lba` to each request's LBA.
- `sector_count()` returns `self.sector_count`.
- `model_string()` returns `"partition N of <inner model>"`.

**PART-API-005 — `PartitionInfo` structure:**
```rust
pub struct PartitionInfo {
    pub number: u32,
    pub start_lba: u64,
    pub end_lba: u64,
    pub size_sectors: u64,
    pub partition_type: u8,
    pub guid_type: Option<[u8; 16]>,
    pub guid_unique: Option<[u8; 16]>,
    pub name: Option<String>,
    pub is_extended: bool,
}
```

---

## Design Notes

- The partition layer is a pure parser — it only reads from the
  `BlockDevice`, never writes. It is safe to call on read-only media.
- GPT UTF-16LE partition names are decoded lossily: unpaired
  surrogates become U+FFFD (REPLACEMENT CHARACTER).
- CRC32 uses the standard IEEE 802.3 polynomial (0xEDB88320), same as
  Ethernet, ZIP, and GPT.
- Extended Boot Record (EBR) logical partition LBAs are interpreted as
  relative to the current EBR's own LBA. The next-EBR pointer is
  interpreted as relative to the extended partition base. This matches
  the convention used by MS-DOS, Windows, and Linux.
- The module is architecture-independent and compiles for both x86_64
  and riscv64.
