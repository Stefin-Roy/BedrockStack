# Kernel Heap Allocator — Invariants

**Version:** 0.3.0
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
- Adjacent coalescing only checks the free list head (not the entire list).
  Non-head coalescing requires a full walk, which is not implemented.
- The `PHYS_ALLOCATOR` static raw pointer is set during `init()` and
  re-pointed after any move of the `BitmapAllocator` via `set_phys_allocator()`.
  This is done at the start of both `Kernel::init()` and `Kernel::run()`.
