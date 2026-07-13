# Physical Frame Allocator — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/mm/phys_alloc.rs`
**Status:** Stable

---

## State Invariants

**ALLOC-001 — Bitmap representation:**
Each bit represents one 4 KiB frame. `1` = allocated/used, `0` = free.
- Location: `kernel/src/mm/phys_alloc.rs:3`

**ALLOC-002 — Bitmap length formula:**
`bitmap_len = (total_frames + 7) / 8` where `total_frames = (max_addr + 4095) / 4096`.
- Location: `kernel/src/mm/phys_alloc.rs:7,59-60`

**ALLOC-003 — Initial bitmap state is fully allocated (`0xFF`):**
Then all frames within `Usable` memory regions are cleared to `0`. This
ensures reserved regions (MMIO, firmware, kernel image) are never allocated.
- Location: `kernel/src/mm/phys_alloc.rs:88-94`

**ALLOC-004 — The bitmap region itself is marked used:**
After clearing usable frames, the bitmap's own frames are re-marked used so
they are never handed out.
- Location: `kernel/src/mm/phys_alloc.rs:96-104`

**ALLOC-005 — Frame 0 (NULL page) is always marked used:**
A raw write unconditionally sets bit 0 after initialization.
- Location: `kernel/src/mm/phys_alloc.rs:106-108`

**ALLOC-006 — `next_free` caches the next candidate frame:**
Linear scan starts from `next_free` on each allocation, updated to `i + 1`
after a successful alloc. On `free()`, `next_free` is lowered if the freed
frame precedes it.
- Location: `kernel/src/mm/phys_alloc.rs:148,217-219`

**ALLOC-007 — `reserve_region` clamps to `total_frames`:**
A region extending beyond the last managed frame is truncated, so a caller
cannot write past the bitmap end.
- Location: `kernel/src/mm/phys_alloc.rs:187-197`

**ALLOC-008 — `alloc_contiguous` finds a run of adjacent free frames:**
Maintains the same `next_free` optimization, advancing past the allocated
run. Returns `None` if no contiguous run of `count` frames exists.
- Location: `kernel/src/mm/phys_alloc.rs:160-179`

---

## Safety Invariants

**ALLOC-S001 — `BitmapAllocator::new` safety:**
`bitmap_region` must be a valid `(base, size)` within a `Usable` memory
region. `memory_map` must describe physical memory accurately.
- Location: `kernel/src/mm/phys_alloc.rs:32-34`

**ALLOC-S002 — `BitmapAllocator::free` safety:**
`addr` must be a frame previously allocated by THIS allocator, and must not
be in use by any other component. Double-free corrupts the bitmap.
- Location: `kernel/src/mm/phys_alloc.rs:209-211`

---

## API Contracts

**ALLOC-API-001 — `alloc()` / `alloc_contiguous()`:**
Returns physical address of a 4 KiB-aligned frame, or `None` if exhausted.
The caller may write to the returned address immediately (identity-mapped
physical RAM).

**ALLOC-API-002 — `reserve_region(start, end)`:**
Marks `[start, end)` as used. `end` may be `u64::MAX` (reserves everything
from `start` to end of managed space). All frames within range are checked
`frame < self.total_frames`.

**ALLOC-API-003 — `managed_end()` vs `alloc_end()`:**
- `managed_end()` = `total_frames * 4096` — the top of the bitmap's address
  range (may include MMIO holes).
- `alloc_end()` = highest address backed by real physical RAM — bounds the
  page-table identity mapping to avoid fabricating tables for nonexistent RAM.
- Location: `kernel/src/mm/phys_alloc.rs:122-133`

---

## Design Notes

- Linear scan is O(n) per allocation. Acceptable because allocations are
  rare vs. user-mode (`alloc`/`dealloc` mostly go through the heap).
- No locking is required because `init()` runs single-threaded and later
  heap growth is serialized by `HEAP.lock()`.
- The bitmap is placed in the largest `Usable` memory region. If that region
  overlaps the kernel image, the bitmap is moved to just after the kernel.
- No cross-CPU allocation is supported (APs don't allocate physical frames
  after boot).
