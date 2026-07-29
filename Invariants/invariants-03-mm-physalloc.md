# Physical Frame Allocator — Invariants

**Version:** 0.3.0
**Source:** `kernel/src/mm/phys_alloc.rs`
**Status:** Stable

---

## State Invariants

**ALLOC-001 — Bitmap representation:**
Each bit represents one 4 KiB frame. `1` = allocated/used, `0` = free.
- Location: `kernel/src/mm/phys_alloc.rs:3`

**ALLOC-002 — Bitmap length formula:**
`bitmap_len = (total_frames + 7) / 8` where `total_frames = (max_addr + 4095) / 4096`.
- Location: `kernel/src/mm/phys_alloc.rs:7,61-62`

**ALLOC-003 — Initial bitmap state is fully allocated (`0xFF`):**
Then all frames within `Usable` memory regions are cleared to `0` via
`clear_region()`. This ensures reserved regions (MMIO, firmware, kernel
image) are never allocated.
- Location: `kernel/src/mm/phys_alloc.rs:89-96`

**ALLOC-004 — The bitmap region itself is marked used:**
After clearing usable frames, the bitmap's own frames are re-marked used via
`mark_region_used()` so they are never handed out.
- Location: `kernel/src/mm/phys_alloc.rs:98-106`

**ALLOC-005 — Frame 0 (NULL page) is always marked used:**
A raw write unconditionally sets bit 0 after initialization.
- Location: `kernel/src/mm/phys_alloc.rs:108-110`

**ALLOC-006 — `next_free` caches the next candidate frame:**
Linear scan starts from `next_free` on each allocation, updated to `idx + 1`
after a successful alloc. On `free()`, `next_free` is lowered if the freed
frame precedes it. Allocation scans 64 bits (one `u64` word) at a time for
~64× throughput.
- Location: `kernel/src/mm/phys_alloc.rs:148-149,151-152,177-178,291-293`

**ALLOC-007 — `reserve_region` clamps to `total_frames`:**
A region extending beyond the last managed frame is truncated, so a caller
cannot write past the bitmap end. A convenience wrapper `reserve_range(addr, size)`
provides `(addr, addr + size)` semantics.
- Location: `kernel/src/mm/phys_alloc.rs:258-277`

**ALLOC-008 — `alloc_contiguous` finds a run of adjacent free frames:**
Maintains the same `next_free` optimization, advancing past the allocated
run. Returns `None` if no contiguous run of `count` frames exists.
- Location: `kernel/src/mm/phys_alloc.rs:219-252`

**ALLOC-009 — `kernel_start`/`kernel_end` bounds provide debug-assurance:**
Both `alloc()` and `alloc_contiguous()` contain `debug_assert` verifying
that the returned frame(s) do not overlap `[kernel_start, kernel_end)`.
This catches use-after-reserve bugs in debug builds.
- Location: `kernel/src/mm/phys_alloc.rs:180-184,204-208,239-243`

**ALLOC-010 — `total_frames()` public accessor:**
Returns `self.total_frames`. Used by VMM setup to allocate intermediate
page-table frames.
- Location: `kernel/src/mm/phys_alloc.rs:141-143`

---

## Safety Invariants

**ALLOC-S001 — `BitmapAllocator::new` safety:**
`bitmap_region` must be a valid `(base, size)` within a `Usable` memory
region. `memory_map` must describe physical memory accurately. The bitmap
placement is adjusted to avoid overlapping `[kernel_start, kernel_end)`.
- Location: `kernel/src/mm/phys_alloc.rs:49-54`

**ALLOC-S002 — `BitmapAllocator::free` safety:**
`addr` must be a frame previously allocated by THIS allocator, and must not
be in use by any other component. Double-free corrupts the bitmap.
- Location: `kernel/src/mm/phys_alloc.rs:284-294`

---

## API Contracts

**ALLOC-API-001 — `alloc()` / `alloc_contiguous()`:**
Returns physical address of a 4 KiB-aligned frame, or `None` if exhausted.
The caller may write to the returned address immediately (identity-mapped
physical RAM). `alloc()` scans 64 bits at a time for performance.

**ALLOC-API-002 — `reserve_region(start, end)`:**
Marks `[start, end)` as used. `end` may be `u64::MAX` (reserves everything
from `start` to end of managed space). All frames within range are checked
`frame < self.total_frames`. `reserve_range(addr, size)` is the convenience
wrapper.

**ALLOC-API-003 — `managed_end()` vs `alloc_end()`:**
- `managed_end()` = `total_frames * 4096` — the top of the bitmap's address
  range (may include MMIO holes).
- `alloc_end()` = highest address backed by real physical RAM — bounds the
  page-table identity mapping to avoid fabricating tables for nonexistent RAM.
- Location: `kernel/src/mm/phys_alloc.rs:126-138`

**ALLOC-API-004 — `total_frames()`:**
Returns the total number of managed 4 KiB frames. Used by VMM and paging
code to size intermediate data structures.

---

## Design Notes

- Linear scan is O(n) per allocation. Acceptable because allocations are
  rare vs. user-mode (`alloc`/`dealloc` mostly go through the heap).
  Scanning 64 bits per word (via inverted `u64`) provides ~64× throughput
  over a bit-by-bit scan.
- No locking is required because `init()` runs single-threaded and later
  heap growth is serialized by `HEAP.lock()`.
- The bitmap is placed in the largest `Usable` memory region preferably
  below 4 GiB (for identity-map compatibility). If that region overlaps the
  kernel image, the bitmap is moved to just after the kernel.
- No cross-CPU allocation is supported (APs don't allocate physical frames
  after boot).
- `clear_region()` and `mark_region_used()` are standalone helper functions
  (not methods) called during `new()`. Both clamp to `total_frames` to
  prevent out-of-bounds bitmap writes.
