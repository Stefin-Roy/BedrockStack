# Kernel Heap Allocator — Invariants

**Version:** 0.4.0
**Source:** `kernel/src/mm/heap.rs`
**Status:** Stable

---

## State Invariants

**HEAP-001 — Free list is singly-linked through `BlockHeader` nodes:**
Each free block starts with a `BlockHeader` containing `size` and `next`
pointer. `null` terminates the list.
- Location: `kernel/src/mm/heap.rs:17-20,41-43`

**HEAP-002 — Allocated blocks store a back-pointer to their header:**
The 8 bytes immediately before the payload contain a `*mut BlockHeader`
pointing to the allocation's header. `BlockHeader::from_payload()` recovers it.
- Location: `kernel/src/mm/heap.rs:22-30`

**HEAP-003 — Adjacent free blocks are coalesced on `push_free()`:**
When a block is freed, it checks if it touches the head block (start of
head == end of block → absorb head), or if the head touches it (end of
head == start of block → head absorbs block). Otherwise prepended to list.
Coalescing only checks the free list head, not the entire list.
- Location: `kernel/src/mm/heap.rs:61-83`

**HEAP-004 — Minimum block size prevents fragmentation deadlock:**
`MIN_BLOCK_SIZE = HEADER_SIZE + BACKPTR_SIZE + MIN_ALLOC`. When splitting,
remaining space < `MIN_BLOCK_SIZE` is consumed entirely rather than creating
a non-splittable fragment.
- Location: `kernel/src/mm/heap.rs:11-12,124`

**HEAP-005 — Heap grows by allocating physical pages from `BitmapAllocator`:**
Initial pool: **256 pages (1 MB)**. Each growth: **16 pages** (or the page
count needed for the current allocation, whichever is larger). If the
allocator returns `None`, growth stops (heap may be exhausted).
- Location: `kernel/src/mm/heap.rs:13-14,219-227,243-244`

**HEAP-006 — `GlobalAlloc` is protected by `spin::Mutex`:**
The `#[global_allocator]` is `HeapAllocator` which wraps `Mutex<HeapInner>`.
Interrupt handlers calling `alloc`/`dealloc` spin-wait if the main thread
holds the lock.
- Location: `kernel/src/mm/heap.rs:170,262-263`

**HEAP-007 — All physical RAM is identity-mapped:**
Heap pages are accessed at their physical addresses (`virtual == physical`)
because the identity map covers `[0, max_addr)`.
- Location: `kernel/src/mm/heap.rs` (implicit — relies on paging invariants)

**HEAP-008 — `set_phys_allocator()` re-points the stashed allocator pointer:**
After the `BitmapAllocator` is moved (into `Kernel`), `set_phys_allocator()`
updates the raw pointer so `allocate_pages()` continues to work. Called at
start of `Kernel::init()` before any heap activity.
- Location: `kernel/src/mm/heap.rs:187-189`

**HEAP-009 — `allocate_pages()` is a helper that grows the heap:**
Calls `phys_allocator().alloc_contiguous(count)` and `heap.add_region()`.
If `alloc_contiguous` fails, a warning is logged and the heap continues
without the extra pages.
- Location: `kernel/src/mm/heap.rs:219-227`

**HEAP-010 — `alloc()` grows by `max(pages_needed, HEAP_GROW_PAGES)`:**
When a `GlobalAlloc::alloc()` call fails to find space in the current free
list, it calculates the minimum pages needed for the requested allocation
(`pages_needed = (layout.size() + 4095) / 4096`) and grows by at least
`HEAP_GROW_PAGES`. This prevents single large resizes from looping.
- Location: `kernel/src/mm/heap.rs:239-249`

**HEAP-011 — Each growth region is tracked as a `ChunkMeta`:**
Every `allocate_pages()` call records `{ vaddr, phys, size, live }` in a
fixed-capacity chunk table (`MAX_CHUNKS = 8192`). `live` counts outstanding
allocations served from that chunk. Exhausting the table panics rather than
overflowing a static array.
- Location: `kernel/src/mm/heap.rs:30-58,439-453`

**HEAP-012 — `live` is incremented/decremented on alloc/free (incl. cache):**
`mark_live`/`unmark_live` are called for every served/freed payload, whether
it came from the shared arena or a per-CPU cache, so chunk accounting stays
balanced regardless of where a block is parked.
- Location: `kernel/src/mm/heap.rs:214-240`, `GlobalAlloc::alloc/dealloc`

**HEAP-013 — Fully-idle chunks are reclaimed opportunistically on `free`:**
`try_reclaim()` frees a chunk's contiguous physical frames and unmaps its VA
range (body + guard) when (a) `live == 0` and (b) the whole body has coalesced
into a single free block occupying exactly the chunk. The lowest (base) chunk
is always reserved. Before scanning, the free list is fully coalesced
(`coalesce_all`), so idle chunks collapse into a single block regardless of
the order the blocks were freed. Blocks parked in per-CPU caches are not part
of the global free list, so chunks with cached blocks are left alone.
- Location: `kernel/src/mm/heap.rs:275-420`

**HEAP-014 — Per-CPU free-list caches over the shared arena:**
Each CPU keeps a private LIFO cache (`PER_CPU_CACHE`, indexed by
`current_cpu_id()`). `alloc` pops a suitably-sized/aligned cached block
first; `dealloc` parks the block in the current CPU's cache. Blocks that
exceed `CPU_CACHE_CAP` fall back to the shared arena. Blocks are
interchangeable — there is no owner-CPU tag — so a block may be freed on any
CPU safely.
- Location: `kernel/src/mm/heap.rs:453-540`

**HEAP-015 — Cache-hits validate size and alignment:**
A cached block is only served if `(header.size + header_addr) - payload >=
layout.size()` and the payload satisfies `layout.align()`. Otherwise the block
is pushed back and the shared arena is used. This prevents handing a caller a
block too small/misaligned for its `Layout`.
- Location: `kernel/src/mm/heap.rs:504-530`

**HEAP-016 — Full-list coalescing on the reclaim path only:**
`coalesce_all()` (selection-sorts the free list by address, then merges
adjacent blocks) runs only inside `try_reclaim()` and only when some non-base
chunk has `live == 0`. It is allocation-free — block headers are re-linked
in place via their `next` pointers — and never runs on the hot alloc/free
path. This makes reclamation independent of free order (unlike the O(1)
head-only coalescing in `push_free`).
- Location: `kernel/src/mm/heap.rs:214-285`

---

## Safety Invariants

**HEAP-S001 — `HeapInner::add_region` safety:**
`start` must point to a valid, writable physical address of at least `size`
bytes that is not in use by any other component.
- Location: `kernel/src/mm/heap.rs:55-59`

**HEAP-S002 — `BlockHeader::from_payload` safety:**
`ptr` must be a valid payload pointer previously returned by
`alloc_inner(layout)` and not yet freed. The back-pointer must be intact.
- Location: `kernel/src/mm/heap.rs:28-30`

**HEAP-S003 — `HeapAllocator::dealloc` safety:**
`ptr` must be a valid allocation from this allocator. Double-free causes
list corruption. The stored `BlockHeader` pointer is trusted.
- Location: `kernel/src/mm/heap.rs:251-259`

**HEAP-S004 — `HeapInner` is `Send + Sync`:**
Justified because all access is serialized through `HEAP.lock()`.
- Location: `kernel/src/mm/heap.rs:45-46`

**HEAP-S005 — `PHYS_ALLOCATOR` raw pointer safety:**
The raw pointer to `BitmapAllocator` is stashed in `init()` and is valid
for the kernel's lifetime because the allocator lives in `Kernel` (pinned
on the stack). Re-pointed via `set_phys_allocator()` after moves.
- Location: `kernel/src/mm/heap.rs:179,187-189,207`

**HEAP-S006 — `try_reclaim` runs with the heap lock held, non-reentrant:**
It unmaps the chunk (which itself may reclaim empty page tables) and calls
`allocator.free()`. No path re-enters `HEAP`, so no deadlock occurs.
- Location: `kernel/src/mm/heap.rs:300-330`

**HEAP-S007 — Per-CPU cache uses payload bytes as link storage:**
While a block is cached, its first 8 bytes (the unused payload region) hold
the next-cache pointer. This is safe only because payloads are ≥ `MIN_ALLOC`
(8) bytes and 8-aligned. The back-pointer at `payload - 8` is never touched
while cached.
- Location: `kernel/src/mm/heap.rs:480-495,504-530`

---

## API Contracts

**HEAP-API-001 — `heap::init(phys)`:**
Must be called exactly once after `BitmapAllocator` is initialized and before
any `alloc`-based code runs. Sets `HEAP_INITIALIZED` and stashes the
physical allocator pointer. Allocates initial 256 pages.
- Location: `kernel/src/mm/heap.rs:204-217`

**HEAP-API-002 — `heap::set_phys_allocator(phys)`:**
Re-points the stashed `PHYS_ALLOCATOR` pointer after the allocator has been
moved. Must be called at the start of `Kernel::init()` and `Kernel::run()`.

**HEAP-API-003 — `GlobalAlloc::alloc(layout)`:**
Returns null if the heap is not initialized or if growth fails. Otherwise
returns a pointer satisfying the requested alignment and size. On failure,
grows by `max(pages_needed, HEAP_GROW_PAGES)` and retries.

**HEAP-API-004 — `GlobalAlloc::dealloc(ptr, layout)`:**
`layout` is unused (only stored on free list). Null pointer is a safe no-op.

---

## Design Notes

- The heap starts with 256 pages (1 MB); the free list grows over the
  physical memory range as allocations trigger `allocate_pages`.
- `alloc_inner` splits free blocks from the START of the block, placing
  any remainder after the allocation. This ensures `BlockHeader` alignment
  of the remainder is preserved.
- Adjacent coalescing on `push_free` checks only the free-list head (O(1)).
  A full coalesce (sort + merge) exists as `coalesce_all()` but runs only on
  the reclaim path, keeping the hot path free of O(n) walks.
- Fully-idle chunks are returned to the physical allocator and their virtual
  ranges unmapped (guard page included); their VAs are not reused (the arena
  grows monotonically downward), but the physical frames are recycled.
- Per-CPU caches are a contention-reduction layer, not an ownership model:
  blocks are interchangeable, so cross-CPU frees are always safe.
- The `PHYS_ALLOCATOR` static raw pointer is set during `init()` and
  re-pointed after any move of the `BitmapAllocator` via `set_phys_allocator()`.
  This is done at the start of both `Kernel::init()` and `Kernel::run()`.
