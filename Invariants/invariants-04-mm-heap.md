# Kernel Heap Allocator — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/mm/heap.rs`
**Status:** Stable

---

## State Invariants

**HEAP-001 — Free list is singly-linked through `BlockHeader` nodes:**
Each free block starts with a `BlockHeader` containing `size` and `next`
pointer. `null` terminates the list.
- Location: `kernel/src/mm/heap.rs:17-20,42-43`

**HEAP-002 — Allocated blocks store a back-pointer to their header:**
The 8 bytes immediately before the payload contain a `*mut BlockHeader`
pointing to the allocation's header. `BlockHeader::from_payload()` recovers it.
- Location: `kernel/src/mm/heap.rs:28-30,151-153`

**HEAP-003 — Adjacent free blocks are coalesced on `push_free()`:**
When a block is freed, it checks if it touches the head block (start of
head == end of block → absorb head), or if the head touches it (end of
head == start of block → head absorbs block). Otherwise prepended to list.
- Location: `kernel/src/mm/heap.rs:61-83`

**HEAP-004 — Minimum block size prevents fragmentation deadlock:**
`MIN_BLOCK_SIZE = HEADER_SIZE + BACKPTR_SIZE + MIN_ALLOC`. When splitting,
remaining space < `MIN_BLOCK_SIZE` is consumed entirely rather than creating
a non-splittable fragment.
- Location: `kernel/src/mm/heap.rs:12-13,124`

**HEAP-005 — Heap grows by allocating physical pages from `BitmapAllocator`:**
Initial pool: 64 pages (256 KB). Each growth: 16 pages. If the allocator
returns `None`, growth stops (heap may be exhausted).
- Location: `kernel/src/mm/heap.rs:13-14,199-212,224-226`

**HEAP-006 — `GlobalAlloc` is protected by `spin::Mutex`:**
The `#[global_allocator]` wraps `Mutex<HeapInner>`. Interrupt handlers
calling `alloc`/`dealloc` spin-wait if the main thread holds the lock.
- Location: `kernel/src/mm/heap.rs:166,243-244`

**HEAP-007 — All physical RAM is identity-mapped:**
Heap pages are accessed at their physical addresses (`virtual == physical`)
because the identity map covers `[0, max_addr)`.
- Location: `kernel/src/mm/heap.rs` (implicit — relies on paging invariants)

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
- Location: `kernel/src/mm/heap.rs:232-241`

**HEAP-S004 — `HeapInner` is `Send + Sync`:**
Justified because all access is serialized through `HEAP.lock()`.
- Location: `kernel/src/mm/heap.rs:45-46`

**HEAP-S005 — `PHYS_ALLOCATOR` raw pointer safety:**
The raw pointer to `BitmapAllocator` is stashed in `init()` and is valid
for the kernel's lifetime because the allocator lives in `Kernel` (pinned
on the stack).
- Location: `kernel/src/mm/heap.rs:169,187`

---

## API Contracts

**HEAP-API-001 — `heap::init(phys)`:**
Must be called exactly once after `BitmapAllocator` is initialized and before
any `alloc`-based code runs. Panics if called twice (no explicit guard, but
`HEAP_INITIALIZED` is set).
- Location: `kernel/src/mm/heap.rs:184-197`

**HEAP-API-002 — `GlobalAlloc::alloc(layout)`:**
Returns null if the heap is not initialized or if growth fails. Otherwise
returns a pointer satisfying the requested alignment and size.

**HEAP-API-003 — `GlobalAlloc::dealloc(ptr, layout)`:**
`layout` is unused (only stored on free list). Null pointer is a safe no-op.

---

## Design Notes

- The heap starts with 64 pages; the free list grows over the physical
  memory range as allocations trigger `allocate_pages`.
- `alloc_inner` splits free blocks from the START of the block, placing
  any remainder after the allocation. This ensures `BlockHeader` alignment
  of the remainder is preserved.
- Adjacent coalescing only checks the free list head (not the entire list).
  Non-head coalescing requires a full walk, which is not implemented.
