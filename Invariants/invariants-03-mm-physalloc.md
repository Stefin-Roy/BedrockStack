# Physical Frame Allocator — Invariants

**Version:** 0.4.0
**Date:** 2026-07-31
**Source:** `kernel/src/mm/phys_alloc.rs`
**Status:** Stable

---

## State Invariants

**ALLOC-001 — Bitmap representation:**
Each bit represents one 4 KiB frame. `1` = allocated/used, `0` = free.
- Location: `kernel/src/mm/phys_alloc.rs:1-10`

**ALLOC-002 — Bitmap length formula:**
`total_frames = (max_addr + 4095) / 4096` where `max_addr` is the highest
`base + size` of any `Usable` region (`find_max_addr`, not a hardcoded
ceiling). `bitmap_len = (total_frames + 7) / 8`.
- Location: `kernel/src/mm/phys_alloc.rs:40-47,72-74`

**ALLOC-003 — Initial bitmap state is fully allocated (`0xFF`):**
Then all frames within `Usable` memory regions are cleared to `0` via
`clear_region()`. This ensures reserved regions (MMIO, firmware, kernel
image) are never allocated.
- Location: `kernel/src/mm/phys_alloc.rs:102-108`

**ALLOC-004 — The bitmap region itself is marked used:**
After clearing usable frames, the bitmap's own frames are re-marked used via
`mark_region_used()` so they are never handed out.
- Location: `kernel/src/mm/phys_alloc.rs:110-118`

**ALLOC-005 — Frame 0 (NULL page) is always marked used:**
A raw write unconditionally sets bit 0 after initialization.
- Location: `kernel/src/mm/phys_alloc.rs:120-122`

**ALLOC-006 — `next_free` caches the next candidate frame:**
Linear scan starts from `next_free` on each allocation, updated to `idx + 1`
after a successful alloc. On `free()`, `next_free` is lowered if the freed
frame precedes it. Allocation scans 64 bits (one `u64` word) at a time for
~64× throughput.
- Location: `kernel/src/mm/phys_alloc.rs:153-178,290-302`

**ALLOC-007 — `reserve_region` clamps to `total_frames`:**
A region extending beyond the last managed frame is truncated, so a caller
cannot write past the bitmap end. A convenience wrapper `reserve_range(addr, size)`
provides `(addr, addr + size)` semantics. The kernel reserves the framebuffer
this way during `Kernel::new()`.
- Location: `kernel/src/mm/phys_alloc.rs:263-281`, `kernel/src/lib.rs:132`

**ALLOC-008 — `alloc_contiguous` finds a run of adjacent free frames:**
Maintains the same `next_free` optimization, advancing past the allocated
run. Returns `None` if no contiguous run of `count` frames exists.
- Location: `kernel/src/mm/phys_alloc.rs:224-256`

**ALLOC-009 — `kernel_start`/`kernel_end` bounds provide debug-assurance:**
Both `alloc()` and `alloc_contiguous()` contain `debug_assert` verifying
that the returned frame(s) do not overlap `[kernel_start, kernel_end)`.
This catches use-after-reserve bugs in debug builds.
- Location: `kernel/src/mm/phys_alloc.rs:185,209,244`

**ALLOC-010 — `total_frames()` public accessor:**
Returns `self.total_frames`. Used by VMM setup to allocate intermediate
page-table frames.
- Location: `kernel/src/mm/phys_alloc.rs:146-148`

**ALLOC-011 — `alloc_end()` is the top of the LAST usable chunk:**
`alloc_end` is the highest address of any usable region (exclusive), not the
end of a contiguous block. Holes (PCI MMIO, framebuffer, etc.) are managed by
never clearing their bits — **there is no separate `managed_end`**; the old
`managed_end()` accessor was removed.
- Location: `kernel/src/mm/phys_alloc.rs:17,128,135-143`

**ALLOC-012 — `BitmapAllocator` is `Send + Sync`:**
`unsafe impl Send`/`Sync` — the allocator is externally synchronized (the
heap's `HEAP` lock, or single-threaded init). This is what lets
`PhysicalMemoryAllocator` be provided as a `Capability`.
- Location: `kernel/src/mm/phys_alloc.rs:23-24,337-365`

**ALLOC-013 — `BitmapAllocator::new()` dumps the full memory map to serial:**
Every region (`[mmap] base/size/end/kind`) plus the derived `max_addr`,
`frames`, `bitmap_len`, and bitmap base are printed at init for debugging.
- Location: `kernel/src/mm/phys_alloc.rs:60-99`

---

## Safety Invariants

**ALLOC-S001 — `BitmapAllocator::new` safety:**
`bitmap_region` must be a valid `(base, size)` within a `Usable` memory
region. `memory_map` must describe physical memory accurately. The bitmap
placement is adjusted to avoid overlapping `[kernel_start, kernel_end)`.
- Location: `kernel/src/mm/phys_alloc.rs:37-39,49-54,84-93`

**ALLOC-S002 — `BitmapAllocator::free` safety:**
`addr` must be a frame previously allocated by THIS allocator, and must not
be in use by any other component. Double-free corrupts the bitmap.
- Location: `kernel/src/mm/phys_alloc.rs:284-302`

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

**ALLOC-API-003 — `alloc_end()`:**
Returns the highest address backed by real physical RAM (top of the last
usable chunk, exclusive). Bounds the page-table identity mapping to avoid
fabricating tables for nonexistent RAM. `managed_end()` no longer exists.

**ALLOC-API-004 — `total_frames()`:**
Returns the total number of managed 4 KiB frames. Used by VMM and paging
code to size intermediate data structures.

**ALLOC-API-005 — `PhysicalMemoryAllocator` capability impl:**
`alloc_frames(count)` maps to `alloc()` (count 1) or `alloc_contiguous(count)`;
`free_frames(addr, _count)` **ignores `count`** — only the frame at `addr` is
freed (per-bit granularity; the caller must free a contiguous run one frame
at a time, or the ignore is safe because contiguity is guaranteed by the
allocator). See `invariants-23-services.md` (SVC-D004).
- Location: `kernel/src/mm/phys_alloc.rs:345-365`

---

## Design Notes

- Linear scan is O(n) per allocation. Acceptable because allocations are
  rare vs. user-mode (`alloc`/`dealloc` mostly go through the heap).
  Scanning 64 bits per word (via inverted `u64`) provides ~64× throughput
  over a bit-by-bit scan.
- The allocator is `Send + Sync`; concurrent access is serialized by the
  heap lock (later growth) or by single-threaded init (early boot).
- The bitmap is placed in the largest `Usable` memory region preferably
  below 4 GiB (for identity-map compatibility). If that region overlaps the
  kernel image, the bitmap is moved to just after the kernel.
- No cross-CPU allocation is supported (APs don't allocate physical frames
  after boot).
- `clear_region()` and `mark_region_used()` are standalone helper functions
  (not methods) called during `new()`. Both clamp to `total_frames` to
  prevent out-of-bounds bitmap writes.
