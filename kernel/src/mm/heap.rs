use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::filesystems::vfs::irq::IrqMutex;
use crate::mm::layout::{HEAP_FLOOR, HEAP_GUARD_BYTES, HEAP_TOP};
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

const HEADER_SIZE: usize = size_of::<BlockHeader>();
const BLOCK_ALIGN: usize = core::mem::align_of::<BlockHeader>();
const BACKPTR_SIZE: usize = size_of::<*mut BlockHeader>();
const MIN_ALLOC: usize = 8;
const MIN_BLOCK_SIZE: usize = HEADER_SIZE + BACKPTR_SIZE + MIN_ALLOC;
const HEAP_INIT_PAGES: usize = 256;
const HEAP_GROW_PAGES: usize = 16;

// Heap chunk reclaim — disabled by default.  The drain+coalesce proof (is_single_free_block
// after drain_cached_blocks+coalesce_all, then unmap_range_collect+free_frames exact count)
// is implemented but not yet stress-verified under SMP shootdown (`HEAP` is `Mutex` not
// `IrqMutex` so an IRQ-time `alloc` miss can spin on `HEAP`). Keep disabled until
// `heap_trace` drain proof and multi-core grow+reclaim stress passes QEMU.
// See invariants-04-mm-heap.md:HEAP-007 / HEAP-013.
const ENABLE_HEAP_CHUNK_RECLAIM: bool = false;

// ── Free-list integrity guards ─────────────────────────────────────────
//
// A free-list walk is O(n) for lookups/removals and O(n²) for the reclaim
// sort.  A double-insert of the same block (double-free, or a corrupted
// header) turns the list into a cycle, which makes every walk loop forever
// with no output — indistinguishable from a hard freeze.  These bounds turn
// that silent hang into a loud diagnostic dump + panic instead.
const FREE_LIST_WALK_BOUND: usize = 1_000_000;
const SORT_OUTER_BOUND: usize = 1_000_000;
const SORT_INNER_BOUND: usize = 50_000_000;

macro_rules! heap_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "heap_trace")]
        $($arg)*
    };
}

#[repr(C)]
struct BlockHeader {
    size: usize,
    next: *mut BlockHeader,
}

impl BlockHeader {
    /// Recover the block header recorded immediately before the payload.
    ///
    /// A header cannot in general be recovered by rounding a payload address:
    /// an allocation with a large alignment can have padding between its
    /// header and payload.  `alloc_inner` stores this back-pointer instead.
    ///
    /// # Safety
    /// `ptr` must be a payload previously returned by `alloc_inner` and not yet
    /// freed. The 8 bytes at `ptr - BACKPTR_SIZE` must hold a valid
    /// `*mut BlockHeader` (written by `alloc_inner`).
    unsafe fn from_payload(ptr: *mut u8) -> *mut BlockHeader {
        debug_assert!(!ptr.is_null(), "from_payload: null pointer");
        debug_assert!(
            (ptr as usize) >= BACKPTR_SIZE,
            "from_payload: ptr too low {:#x}",
            ptr as usize
        );
        let header_ptr = unsafe { *((ptr as usize - BACKPTR_SIZE) as *const *mut BlockHeader) };
        debug_assert!(!header_ptr.is_null(), "from_payload: header was null");
        header_ptr
    }

    fn end(&self) -> usize {
        (self as *const Self as usize) + self.size
    }
}

/// One mapped heap growth region (a contiguous physical block).
///
/// Tracks how many live allocations are served from the region so that a
/// fully-idle chunk can be unmapped and its frames returned to the physical
/// allocator (`try_reclaim`).
#[derive(Clone, Copy, Debug)]
struct ChunkMeta {
    vaddr: u64,
    phys: u64,
    size: u64,
    live: usize,
}

impl ChunkMeta {
    const ZERO: Self = Self {
        vaddr: 0,
        phys: 0,
        size: 0,
        live: 0,
    };
}

/// Upper bound on the number of tracked heap growth regions.  The heap arena
/// is 512 MiB and the smallest growth a region can occupy (with its unmapped
/// guard page) is HEAP_GROW_PAGES + 1 pages (~64 KiB), so the arena can fit
/// ~8192 regions at most.  If this is ever exhausted we panic rather than
/// overflow a static array.
const MAX_CHUNKS: usize = 8192;

/// One fully-idle chunk scheduled for unmapping.
///
/// Records are collected while the heap lock is held, then unmapped after the
/// lock is released.  VMM unmapping performs a cross-CPU TLB shootdown and
/// must not wait for another CPU that is blocked on this heap lock.
#[derive(Clone, Copy)]
struct ReclaimRecord {
    vaddr: u64,
    phys: u64,
    size: u64,
}

impl ReclaimRecord {
    const ZERO: Self = Self {
        vaddr: 0,
        phys: 0,
        size: 0,
    };
}

/// Upper bound on chunks reclaimed per call.  Reclaiming is best-effort: if
/// more chunks are reclaimable than fit, the remainder stays mapped and is
/// picked up by the next live == 0 transition.
const MAX_RECLAIMS_PER_CALL: usize = 64;

pub struct HeapInner {
    free_list: *mut BlockHeader,
    /// Lowest virtual address currently committed to a heap chunk. Guard
    /// pages are left unmapped between successive chunks.
    low_vaddr: u64,
    /// Active page-table root (PML4/Sv39 L2) used to map heap growth.
    root: u64,
    /// Tracked growth regions (newest, lowest chunk first).
    chunks: [ChunkMeta; MAX_CHUNKS],
    chunk_count: usize,
}

unsafe impl Send for HeapInner {}
unsafe impl Sync for HeapInner {}

impl HeapInner {
    pub const fn empty() -> Self {
        HeapInner {
            free_list: core::ptr::null_mut(),
            low_vaddr: u64::MAX,
            root: 0,
            chunks: [ChunkMeta::ZERO; MAX_CHUNKS],
            chunk_count: 0,
        }
    }

    pub unsafe fn add_region(&mut self, start: usize, size: usize) {
        let block = start as *mut BlockHeader;
        unsafe {
            *block = BlockHeader {
                size,
                next: core::ptr::null_mut(),
            }
        }
        self.push_free(block);
    }

    fn push_free(&mut self, block: *mut BlockHeader) {
        let block_ref = unsafe { &mut *block };
        block_ref.next = core::ptr::null_mut();

        // Insert in address order so the list stays sorted at all times.
        // Keeps first-fit effective (it walks from the lowest address) and
        // lets every free coalesce with both neighbours, so the list never
        // fragments into the scattered order that used to need an O(n log n)
        // rebuild on the reclaim path.
        let mut prev: *mut BlockHeader = core::ptr::null_mut();
        let mut cur = self.free_list;
        let mut steps = 0usize;
        while !cur.is_null() && (cur as usize) < (block as usize) {
            steps += 1;
            if steps > FREE_LIST_WALK_BOUND {
                self.free_list_fault("push_free");
            }
            prev = cur;
            cur = unsafe { (*cur).next };
        }

        // Coalesce with the following neighbour (`block` immediately precedes
        // `cur`).
        if !cur.is_null() {
            let cur_ref = unsafe { &*cur };
            if block_ref.end() == cur as usize {
                block_ref.size += cur_ref.size;
                block_ref.next = cur_ref.next;
            } else {
                block_ref.next = cur;
            }
        }

        // Coalesce with the preceding neighbour (`prev` immediately precedes
        // `block`); `prev` absorbs `block`.
        if !prev.is_null() {
            let prev_ref = unsafe { &mut *prev };
            if prev_ref.end() == block as usize {
                prev_ref.size += block_ref.size;
                prev_ref.next = block_ref.next;
                return;
            }
            prev_ref.next = block;
        } else {
            self.free_list = block;
        }
    }

    fn remove_next(&mut self, prev: *mut BlockHeader) {
        if prev.is_null() {
            // Remove head.
            if !self.free_list.is_null() {
                let head = unsafe { &*self.free_list };
                self.free_list = head.next;
            }
        } else {
            let prev_ref = unsafe { &*prev };
            if !prev_ref.next.is_null() {
                let target = unsafe { &*prev_ref.next };
                let prev_mut = unsafe { &mut *prev };
                prev_mut.next = target.next;
            }
        }
    }

    // ── Chunk tracking ────────────────────────────────────────────

    /// Record a freshly mapped growth region of `size` bytes at `vaddr`
    /// backed by contiguous physical frames starting at `phys`.
    fn register_chunk(&mut self, vaddr: u64, phys: u64, size: u64) {
        assert!(
            self.chunk_count < MAX_CHUNKS,
            "heap: chunk table exhausted ({MAX_CHUNKS}), arena too fragmented to track"
        );
        self.chunks[self.chunk_count] = ChunkMeta {
            vaddr,
            phys,
            size,
            live: 0,
        };
        self.chunk_count += 1;
    }

    /// Index of the chunk containing `addr`, or `None`.
    fn chunk_idx(&self, addr: u64) -> Option<usize> {
        if addr == 0 {
            return None;
        }
        for (i, c) in self.chunks[..self.chunk_count].iter().enumerate() {
            if addr >= c.vaddr && addr < c.vaddr + c.size {
                return Some(i);
            }
        }
        None
    }

    /// Increment the outstanding-allocation count of the chunk at `addr`.
    ///
    /// # Safety
    /// `addr` must be a just-served payload lying within a registered chunk.
    fn mark_live(&mut self, addr: u64) {
        if let Some(i) = self.chunk_idx(addr) {
            let c = &mut self.chunks[i];
            c.live = c.live.saturating_add(1);
        }
    }

    /// Decrement the outstanding-allocation count of the chunk at `addr`.
    ///
    /// Returns the chunk index and its new live count, so the caller can
    /// detect the live == 0 transition without re-scanning for the chunk.
    fn unmark_live(&mut self, addr: u64) -> Option<(usize, usize)> {
        let i = self.chunk_idx(addr)?;
        let c = &mut self.chunks[i];
        c.live = c.live.saturating_sub(1);
        Some((i, c.live))
    }

    /// True when the chunk at `i` is fully idle and not the base chunk that
    /// anchors the arena (the base chunk is never reclaimed).
    fn chunk_is_reclaimable(&self, i: usize) -> bool {
        let c = &self.chunks[i];
        c.live == 0 && c.vaddr != self.low_vaddr
    }

    /// True when the free list contains a single block header at `addr` whose
    /// size exactly spans `size` bytes (i.e. the body is one coalesced block).
    fn is_single_free_block(&self, addr: u64, size: u64) -> bool {
        let mut cur = self.free_list;
        let mut steps = 0usize;
        while !cur.is_null() {
            steps += 1;
            if steps > FREE_LIST_WALK_BOUND {
                self.free_list_fault("is_single_free_block");
            }
            if cur as usize == addr as usize {
                return unsafe { (*cur).size as u64 } == size;
            }
            cur = unsafe { (*cur).next };
        }
        false
    }

    /// Remove the free block starting exactly at `addr` from the free list.
    fn remove_free_block(&mut self, addr: u64) -> bool {
        let mut prev: *mut BlockHeader = core::ptr::null_mut();
        let mut cur = self.free_list;
        let mut steps = 0usize;
        while !cur.is_null() {
            steps += 1;
            if steps > FREE_LIST_WALK_BOUND {
                self.free_list_fault("remove_free_block");
            }
            if cur as usize == addr as usize {
                self.remove_next(prev);
                return true;
            }
            prev = cur;
            cur = unsafe { (*cur).next };
        }
        false
    }

    /// Diagnose a corrupted (cyclic or absurdly long) free list, dump up to 64
    /// nodes from the head so the fault is visible on serial, then abort.
    fn free_list_fault(&self, tag: &str) -> ! {
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("\n[heap] FREE-LIST FAULT in ");
        SerialPort::puts(tag);
        SerialPort::puts(": first 64 nodes from head:\n");
        let mut cur = self.free_list;
        let mut n = 0u64;
        while n < 64 && !cur.is_null() {
            SerialPort::puts("  node@0x");
            SerialPort::put_hex(cur as u64);
            SerialPort::puts(" size=0x");
            SerialPort::put_hex(unsafe { (*cur).size } as u64);
            SerialPort::puts(" next=0x");
            SerialPort::put_hex(unsafe { (*cur).next } as u64);
            SerialPort::puts("\n");
            cur = unsafe { (*cur).next };
            n += 1;
        }
        panic!("heap: free-list fault in {}", tag);
    }

    // ── Full-list coalescing ───────────────────────────────────────
    //
    // `push_free` only coalesces against the free-list head (O(1), but
    // ineffective for scattered free orders).  For chunk reclamation we need
    // the whole body of an idle chunk to collapse into ONE free block, so we
    // occasionally rebuild the free list into a fully-merged, address-sorted
    // form.  This runs only on the rare reclaim path (when a chunk is fully
    // idle), never on the hot alloc/free path.

    /// Rebuild the free list in-place, sorted by block start address.
    ///
    /// Bottom-up merge sort (width-doubling): O(n log n), allocation-free —
    /// headers are just re-linked.  `SORT_OUTER_BOUND` guards the number of
    /// passes, `SORT_INNER_BOUND` the total node visits; a cyclic list turns
    /// into a free-list fault instead of a silent hang.
    fn sort_free_list(&mut self) {
        let mut list = self.free_list;
        if list.is_null() {
            return;
        }

        let mut width = 1usize;
        let mut outer = 0usize;
        let mut inner_total = 0usize;

        loop {
            outer += 1;
            if outer > SORT_OUTER_BOUND || inner_total > SORT_INNER_BOUND {
                self.free_list_fault("sort");
            }

            let mut cur = list;
            let mut new_head: *mut BlockHeader = core::ptr::null_mut();
            let mut new_tail: *mut BlockHeader = core::ptr::null_mut();
            let mut runs = 0usize;

            while !cur.is_null() {
                runs += 1;

                // Run 1: up to `width` nodes.
                let run1 = cur;
                let mut i = 0usize;
                while i + 1 < width && !cur.is_null() {
                    inner_total += 1;
                    if inner_total > SORT_INNER_BOUND {
                        self.free_list_fault("sort");
                    }
                    cur = unsafe { (*cur).next };
                    i += 1;
                }
                if cur.is_null() {
                    // run1 is the last run: append it as-is and finish the pass.
                    if new_tail.is_null() {
                        new_head = run1;
                    } else {
                        unsafe {
                            (*new_tail).next = run1;
                        }
                    }
                    break;
                }

                // Run 2: up to `width` nodes (may be empty).
                let run2 = unsafe { (*cur).next };
                unsafe {
                    (*cur).next = core::ptr::null_mut();
                }
                cur = run2;
                if cur.is_null() {
                    // Only one run in the whole list: append it as-is.
                    if new_tail.is_null() {
                        new_head = run1;
                    } else {
                        unsafe {
                            (*new_tail).next = run1;
                        }
                    }
                    break;
                }
                i = 0;
                while i + 1 < width && !cur.is_null() {
                    inner_total += 1;
                    if inner_total > SORT_INNER_BOUND {
                        self.free_list_fault("sort");
                    }
                    cur = unsafe { (*cur).next };
                    i += 1;
                }
                let next_run_head = if cur.is_null() {
                    core::ptr::null_mut()
                } else {
                    unsafe { (*cur).next }
                };
                if !cur.is_null() {
                    unsafe {
                        (*cur).next = core::ptr::null_mut();
                    }
                }

                let (merged, merged_tail) = self.merge_runs(run1, run2, &mut inner_total);
                if new_tail.is_null() {
                    new_head = merged;
                } else {
                    unsafe {
                        (*new_tail).next = merged;
                    }
                }
                new_tail = merged_tail;
                cur = next_run_head;
            }

            list = new_head;
            if runs <= 1 {
                break;
            }
            width = width.saturating_mul(2);
            if width == 0 {
                self.free_list_fault("sort");
            }
        }

        self.free_list = list;
    }

    /// Merge two already-sorted free runs by start address into one sorted
    /// run.  Returns the merged run's head and tail.  Allocation-free: only
    /// `next` pointers are rewritten.
    fn merge_runs(
        &self,
        mut a: *mut BlockHeader,
        mut b: *mut BlockHeader,
        inner_total: &mut usize,
    ) -> (*mut BlockHeader, *mut BlockHeader) {
        let mut head: *mut BlockHeader = core::ptr::null_mut();
        let mut tail: *mut BlockHeader = core::ptr::null_mut();
        while !a.is_null() && !b.is_null() {
            *inner_total += 1;
            if *inner_total > SORT_INNER_BOUND {
                self.free_list_fault("sort");
            }
            let take_a = (a as usize) < (b as usize);
            let node = if take_a { a } else { b };
            if tail.is_null() {
                head = node;
            } else {
                unsafe {
                    (*tail).next = node;
                }
            }
            tail = node;
            if take_a {
                a = unsafe { (*a).next };
            } else {
                b = unsafe { (*b).next };
            }
        }
        // Attach the remaining tail of whichever run is not exhausted.
        let mut rest = if a.is_null() { b } else { a };
        if tail.is_null() {
            head = rest;
        } else {
            unsafe {
                (*tail).next = rest;
            }
        }
        while !rest.is_null() && !unsafe { (*rest).next }.is_null() {
            *inner_total += 1;
            if *inner_total > SORT_INNER_BOUND {
                self.free_list_fault("sort");
            }
            rest = unsafe { (*rest).next };
        }
        (head, rest)
    }

    /// Single pass over the (address-sorted) free list merging adjacent
    /// blocks.  Keeps the lower-address header as the merged block.
    fn merge_sorted_free_blocks(&mut self) {
        let mut cur = self.free_list;
        let mut steps = 0usize;
        while !cur.is_null() {
            steps += 1;
            if steps > FREE_LIST_WALK_BOUND {
                self.free_list_fault("merge");
            }
            let next = unsafe { (*cur).next };
            if !next.is_null() {
                let cur_end = (cur as usize) + unsafe { (*cur).size };
                if cur_end == next as usize {
                    // `next` is physically adjacent after `cur`: absorb it.
                    unsafe {
                        (*cur).size += (*next).size;
                        (*cur).next = (*next).next;
                    }
                    // Keep `cur` fixed; the new `cur.next` may be adjacent too.
                    continue;
                }
            }
            cur = unsafe { (*cur).next };
        }
    }

    /// Sort + merge the free list so every contiguous free region is one block.
    fn coalesce_all(&mut self) {
        self.sort_free_list();
        self.merge_sorted_free_blocks();
    }

    /// True when some idle chunk's on-list free blocks cover its whole body.
    ///
    /// A single O(L) walk per idle chunk (trigger chunk first, then the rest)
    /// sums the sizes of free blocks lying within each idle chunk's bounds and
    /// stops early once a chunk's sum reaches `size`.  Runs before the sort so
    /// the O(n log n) coalesce never runs fruitlessly: after `drain_cached_blocks`
    /// a genuinely idle chunk is fully covered iff it is truly reclaimable.
    ///
    /// This is also the SMP correctness gate: a block another CPU popped from a
    /// cache but not yet re-marked live sits neither on the free list nor in any
    /// cache, so coverage fails and the chunk is left mapped instead of being
    /// unmapped with a block in flight.
    fn any_idle_chunk_covered(&self, base: u64, trigger_idx: usize) -> bool {
        // Visit every chunk exactly once, starting with the trigger chunk, so
        // the common single-idle-chunk case is a single O(L) walk.  Wrapping
        // modulo keeps this allocation-free regardless of chunk_count.
        for step in 0..self.chunk_count {
            let i = (trigger_idx + step) % self.chunk_count;
            let c = &self.chunks[i];
            if c.live != 0 || c.vaddr == base {
                continue;
            }
            let vaddr = c.vaddr;
            let end = vaddr.saturating_add(c.size);
            let mut sum: u64 = 0;
            let mut cur = self.free_list;
            let mut steps = 0usize;
            while !cur.is_null() {
                steps += 1;
                if steps > FREE_LIST_WALK_BOUND {
                    self.free_list_fault("coverage");
                }
                let block = unsafe { &*cur };
                let addr = cur as u64;
                if addr >= vaddr && addr < end {
                    sum += block.size as u64;
                    if sum >= c.size {
                        return true;
                    }
                }
                cur = block.next;
            }
        }
        false
    }

    /// Move every cached block belonging to chunk `chunk_idx` back onto the
    /// shared free list.
    ///
    /// While a block sits in a per-CPU cache its payload's first bytes hold the
    /// cache link and its header `next` is stale, so each drained block is
    /// re-linked (`.next = null`) before `push_free`.  Called with the heap lock
    /// held; taking the per-CPU cache locks in this order (heap → cache) cannot
    /// deadlock because no path ever takes the heap lock while holding a cache
    /// lock — `per_cpu_alloc_cached`/`per_cpu_free_cached` release theirs before
    /// the caller takes the heap lock.
    fn drain_cached_blocks(&mut self, chunk_idx: usize) {
        let (vaddr, size) = {
            let c = &self.chunks[chunk_idx];
            (c.vaddr, c.vaddr.saturating_add(c.size))
        };
        for cpu in 0..crate::smp::MAX_CPUS {
            let mut cache = PER_CPU_CACHE[cpu].lock();
            let mut prev: *mut u8 = core::ptr::null_mut();
            let mut cur = cache.head;
            while !cur.is_null() {
                // The cached link lives in the payload's first 8 bytes.
                let next = unsafe { *(cur as *const *mut u8) };
                let addr = cur as u64;
                if addr >= vaddr && addr < size {
                    if prev.is_null() {
                        cache.head = next;
                    } else {
                        unsafe {
                            *(prev as *mut *mut u8) = next;
                        }
                    }
                    cache.count -= 1;
                    let block = unsafe { BlockHeader::from_payload(cur) };
                    unsafe {
                        (*block).next = core::ptr::null_mut();
                    }
                    self.push_free(block);
                } else {
                    prev = cur;
                }
                cur = next;
            }
        }
    }

    /// One fully-idle chunk scheduled for unmapping.
    ///
    /// Reclaim records are collected while the heap lock is held, then the caller
    /// drops the lock before running `reclaim_unmapped` on each.  The unmap must
    /// NOT happen under the heap lock: `vmm.unmap` performs a cross-CPU TLB
    /// shootdown that waits for every online CPU to acknowledge, and another CPU
    /// blocked on the heap lock with interrupts disabled (e.g. inside an `IrqMutex`
    /// VFS critical section) could never service that IPI — a deadlock.
    /// Best-effort reclaim of fully-idle growth regions.
    ///
    /// A chunk is reclaimed only when all its allocations are freed (`live ==
    /// 0`) and its whole body has coalesced into a single free block.  The
    /// lowest (base) chunk, which anchors the arena, is always reserved.
    ///
    /// Callers invoke this exactly when a chunk transitions to `live == 0`, so
    /// the run is deterministic (one-shot per transition, not on every cache
    /// overflow).  Before scanning, idle chunks' cached blocks are drained and
    /// the free list is fully coalesced so an idle chunk collapses into a
    /// single block; a coverage pre-check gates the sort so it never runs
    /// fruitlessly.
    ///
    /// `trigger_idx` is the chunk that just became idle; it is checked first by
    /// the coverage pre-check so the common case is a single O(L) walk.
    ///
    /// Reclaimed chunks are NOT unmapped here: their (vaddr, phys, size) records
    /// are written to `out` and the caller must release the heap lock before
    /// unmapping them (see [`ReclaimRecord`]).
    ///
    /// Returns the number of records written to `out`.
    ///
    /// # Safety
    /// Must be called with the heap lock held and not re-entrant.
    fn try_reclaim(&mut self, trigger_idx: usize, out: &mut [ReclaimRecord]) -> usize {
        let base = self.low_vaddr;

        // Fast bail-out: nothing is idle, so skip the expensive coalesce.
        let has_idle = self.chunks[..self.chunk_count]
            .iter()
            .any(|c| c.live == 0 && c.vaddr != base);
        if !has_idle {
            return 0;
        }
        heap_trace!(crate::drivers::serial::dump_puts(
            "[heap-trace] try_reclaim enter: chunks="
        ));
        heap_trace!(crate::drivers::serial::dump_put_u64(
            self.chunk_count as u64
        ));
        heap_trace!(crate::drivers::serial::dump_puts("\n"));

        // Bring idle chunks' cached blocks back onto the free list so their
        // bodies can genuinely collapse to a single block.
        for i in 0..self.chunk_count {
            if self.chunk_is_reclaimable(i) {
                self.drain_cached_blocks(i);
            }
        }

        // Gate the sort: only run it if some idle chunk's on-list free bytes
        // now cover its whole body (e.g. after the drain, or if the chunk is
        // truly reclaimable).  Otherwise return early — no fruitless O(n log n)
        // rebuild, and no reclaim of a chunk with a block in flight.
        if !self.any_idle_chunk_covered(base, trigger_idx) {
            return 0;
        }

        // Merge scattered free blocks so idle chunks form a single block.
        self.coalesce_all();
        heap_trace!(crate::drivers::serial::dump_puts(
            "[heap-trace] try_reclaim: coalesce done\n"
        ));

        let mut i = 0;
        let mut n = 0usize;
        while i < self.chunk_count {
            let candidate = {
                let c = &self.chunks[i];
                c.live == 0 && c.vaddr != base
            };
            if !candidate {
                i += 1;
                continue;
            }
            let (vaddr, phys, size) = {
                let c = &self.chunks[i];
                (c.vaddr, c.phys, c.size)
            };
            if !self.is_single_free_block(vaddr, size) {
                i += 1;
                continue;
            }

            // Leave this candidate intact when the caller's fixed record
            // buffer is full.  Removing its free-list node without also
            // removing the chunk metadata would make the region unreachable
            // to both the allocator and the reclaim pass.
            if n == out.len() {
                break;
            }
            self.remove_free_block(vaddr);

            // Record the chunk for out-of-lock unmapping; stop collecting once
            // the caller's buffer is full (remainder reclaimed next time).
            out[n] = ReclaimRecord { vaddr, phys, size };
            n += 1;

            // Shift trailing chunks down over the removed entry.
            self.chunks.copy_within(i + 1..self.chunk_count, i);
            self.chunk_count -= 1;
            // Do not advance `i`: the entry now at `i` is untested.
        }
        heap_trace!(crate::drivers::serial::dump_puts(
            "[heap-trace] try_reclaim exit: chunks="
        ));
        heap_trace!(crate::drivers::serial::dump_put_u64(
            self.chunk_count as u64
        ));
        heap_trace!(crate::drivers::serial::dump_puts("\n"));
        n
    }

    /// Unmap and release records produced by [`try_reclaim`].
    ///
    /// This must run after the `HEAP` mutex guard has been dropped.  The VMM
    /// performs a cross-CPU TLB shootdown and the physical allocator is also
    /// shared; neither operation may be performed while another CPU can be
    /// forced to wait on the heap lock.
    fn reclaim_unmapped(root: u64, records: &[ReclaimRecord]) {
        if records.is_empty() {
            return;
        }

        let pa = get_phys_allocator_mut();
        let mut vmm = Vmm::from_root(root);
        for record in records {
            heap_trace!(crate::drivers::serial::dump_puts(
                "[heap-trace] reclaim: unmap vaddr="
            ));
            heap_trace!(crate::drivers::serial::dump_put_hex(record.vaddr));
            heap_trace!(crate::drivers::serial::dump_puts("\n"));

            // Unmap the body plus the trailing guard page.  The guard is
            // normally already unmapped; VMM::unmap safely ignores it.
            vmm.unmap(&mut *pa, record.vaddr, record.size + HEAP_GUARD_BYTES);

            // Return the contiguous data frames after the TLB shootdown has
            // completed and no CPU can retain a stale heap mapping.
            let end = record.phys.saturating_add(record.size);
            let mut f = record.phys;
            while f < end {
                unsafe {
                    pa.free(f);
                }
                f += 4096;
            }

            SerialPort::puts("[heap] reclaim chunk vaddr=");
            SerialPort::put_hex(record.vaddr);
            SerialPort::puts(" phys=");
            SerialPort::put_hex(record.phys);
            SerialPort::puts(" size=");
            SerialPort::put_hex(record.size);
            SerialPort::puts(" live=0\n");
        }
    }

    fn alloc_inner(&mut self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(BLOCK_ALIGN);
        let needed = layout.size().max(MIN_ALLOC);

        let mut prev: *mut BlockHeader = core::ptr::null_mut();
        let mut curr = self.free_list;

        while !curr.is_null() {
            let size = unsafe { (*curr).size };
            let next = unsafe { (*curr).next };
            let block_addr = curr as usize;
            let Some(block_end) = block_addr.checked_add(size) else {
                continue;
            };
            let Some(payload_addr) = block_addr
                .checked_add(HEADER_SIZE + BACKPTR_SIZE + align - 1)
                .map(|v| v & !(align - 1))
            else {
                continue;
            };
            let Some(payload_end) = payload_addr.checked_add(needed) else {
                continue;
            };
            let Some(alloc_end) = payload_end
                .checked_add(BLOCK_ALIGN - 1)
                .map(|v| v & !(BLOCK_ALIGN - 1))
            else {
                continue;
            };

            if alloc_end <= block_end {
                let remaining = block_end - alloc_end;

                if remaining >= MIN_BLOCK_SIZE {
                    // Split: allocate from the start of `curr`, replacing it
                    // in the free list with the remainder.  Do not use
                    // `push_free` here: it makes the remainder the head and
                    // `remove_next(prev)` would then remove that remainder,
                    // leaving the allocated block on the free list.
                    let alloc_size = alloc_end - block_addr;
                    unsafe {
                        (*curr).size = alloc_size;
                    }

                    let remainder_addr = block_addr + alloc_size;
                    let remainder = remainder_addr as *mut BlockHeader;
                    unsafe {
                        *remainder = BlockHeader {
                            size: remaining,
                            next,
                        };
                    }
                    if prev.is_null() {
                        self.free_list = remainder;
                    } else {
                        unsafe {
                            (*prev).next = remainder;
                        }
                    }
                } else {
                    // The tail is too small to become a valid free block, so
                    // consume the whole block.
                    self.remove_next(prev);
                }

                // Keep the allocation header address explicitly.  The
                // payload may be more strictly aligned than the header.
                unsafe {
                    ((payload_addr - BACKPTR_SIZE) as *mut *mut BlockHeader).write(curr);
                }
                return payload_addr as *mut u8;
            }

            prev = curr;
            curr = next;
        }

        core::ptr::null_mut()
    }
}

static HEAP_INITIALIZED: AtomicBool = AtomicBool::new(false);
// Keep plain `Mutex` (not `IrqMutex`) for the global heap: growth may
// trigger `VMM::map` → `shootdown_tlb()` which waits for IPI acks. An
// `IrqMutex` would hold IRQs disabled across that wait, deadlocking if the
// target CPU is spinning on `HEAP` with IRQs off (e.g. inside a `VFS IrqMutex`).
// The per-CPU cache (below) *is* `IrqMutex` so IRQ-time `free` never spins.
static HEAP: Mutex<HeapInner> = Mutex::new(HeapInner::empty());

/// Raw pointer to the physical allocator, stashed so `alloc()` can grow the heap.
///
/// # Safety
/// The caller must ensure the pointed-to `BitmapAllocator` outlives the
/// kernel heap (i.e. lives for the entire remaining boot sequence).  This
/// pointer is set once by `init()`, but may be *updated* after a move with
/// `set_phys_allocator()`.
///
/// Uses `AtomicPtr` for lock-free interior mutability – replaces the prior
/// `Once<SyncUnsafeCell>` which relied on unsafe `Sync` impl.
static PHYS_ALLOCATOR: AtomicPtr<BitmapAllocator> = AtomicPtr::new(core::ptr::null_mut());


/// Update the physical allocator pointer after a move.
///
/// The heap stashes a raw pointer to the physical allocator during `init()`,
/// but the allocator struct may be moved (e.g. into `Kernel`).  Call this
/// once the final location is known so heap growth and DMA allocations
/// continue to work.
pub fn set_phys_allocator(phys: &mut BitmapAllocator) {
    let new_ptr = phys as *mut BitmapAllocator;
    let old = PHYS_ALLOCATOR.swap(new_ptr, Ordering::Relaxed);
    if !old.is_null() && old != new_ptr {
        SerialPort::puts("[heap] set_phys_allocator re-point ");
        SerialPort::put_hex(old as u64);
        SerialPort::puts(" -> ");
        SerialPort::put_hex(new_ptr as u64);
        SerialPort::puts("\n");
    }
    crate::acpi::update_alloc(new_ptr);
    crate::services::dma::update_dma_alloc(new_ptr);
}

/// Return a raw pointer to the physical allocator (may be null if uninitialised).
pub fn phys_allocator_raw() -> *mut BitmapAllocator {
    PHYS_ALLOCATOR.load(Ordering::Relaxed)
}

/// Return a mutable reference to the physical allocator.
///
/// # Panics
/// Panics if the allocator has not been initialised yet.
pub fn get_phys_allocator_mut() -> &'static mut BitmapAllocator {
    let ptr = PHYS_ALLOCATOR.load(Ordering::Relaxed);
    if ptr.is_null() {
        SerialPort::puts("[heap] FATAL: no physical allocator\n");
        loop {}
    }
    unsafe { &mut *ptr }
}

unsafe fn phys_allocator() -> &'static mut BitmapAllocator {
    get_phys_allocator_mut()
}

/// Initialise the kernel heap.
///
/// The heap lives in the dedicated `HEAP` arena (above `KERNEL_VMA_BASE`);
/// growth chunks are mapped there with an unmapped guard page between them.
/// Physical frames are allocated from `phys` and mapped read/write + NX.
///
/// # Safety
/// Must be called exactly once, after the physical allocator is ready and the
/// kernel page tables are live (`root` is the active table root).
pub unsafe fn init(root: u64, phys: &mut BitmapAllocator) {
    SerialPort::puts("[heap] init\n");

    let ptr = phys as *mut BitmapAllocator;
    PHYS_ALLOCATOR.store(ptr, Ordering::Relaxed);

    let mut heap = HEAP.lock();
    heap.root = root;
    allocate_pages(&mut heap, HEAP_INIT_PAGES);
    HEAP_INITIALIZED.store(true, Ordering::SeqCst);
    SerialPort::puts("[heap] init done, pages=0x");
    SerialPort::put_hex(HEAP_INIT_PAGES as u64);
    SerialPort::puts(" arena=[0x");
    SerialPort::put_hex(heap.low_vaddr);
    SerialPort::puts(",0x");
    SerialPort::put_hex(HEAP_TOP);
    SerialPort::puts(")\n");
}

fn allocate_pages(heap: &mut HeapInner, count: usize) {
    let phys = unsafe { phys_allocator() };

    let pa = match phys.try_alloc_contiguous(count) {
        Ok(pa) => pa,
        Err(crate::mm::phys_alloc::AllocError::NoFrames) => {
            SerialPort::puts("[heap] WARN: NoFrames for heap growth count=");
            SerialPort::put_u64(count as u64);
            SerialPort::puts(" free=");
            SerialPort::put_u64(phys.free_frames() as u64);
            SerialPort::puts("\n");
            return;
        }
        Err(crate::mm::phys_alloc::AllocError::InvalidCount) => {
            SerialPort::puts("[heap] WARN: InvalidCount for heap growth\n");
            return;
        }
    };

    let size = (count as u64) * 4096;
    // Guard page separates this new chunk from the one below (or the top
    // of the arena for the first chunk).
    let low = if heap.low_vaddr == u64::MAX {
        HEAP_TOP
            .checked_sub(size)
            .expect("heap: initial chunk exceeds arena")
    } else {
        heap.low_vaddr
            .checked_sub(HEAP_GUARD_BYTES)
            .and_then(|v| v.checked_sub(size))
            .expect("heap: chunk underflow below arena floor")
    };
    assert!(low >= HEAP_FLOOR, "heap: arena exhausted at {:#x}", low);

    let mut vmm = Vmm::from_root(heap.root);
    let alloc = &mut *phys;
    vmm.map(alloc, low, pa, size, PageFlags::READ | PageFlags::WRITE);
    unsafe {
        heap.add_region(low as usize, size as usize);
    }
    heap.low_vaddr = low;
    heap.register_chunk(low, pa, size);
}

// ── Per-CPU free-list caches ───────────────────────────────────────
//
// The shared arena (`HEAP`) is protected by a mutex, so every allocation/
// free contends on it.  To cut that contention, each CPU keeps a small
// private LIFO cache of freed blocks.  A block can be (de)allocated on any
// CPU safely — the cache is just a staging area: blocks that overflow the
// cache (or misses) fall back to the shared arena.  Freeing always lands in
// the *current* CPU's cache, which is fine because blocks are interchangeable;
// there is no owner-CPU tag to preserve.

/// Maximum number of freed blocks a single CPU caches before excess returns
/// to the shared arena.
const CPU_CACHE_CAP: usize = 64;

/// A simple intrusive LIFO stack of freed payload pointers.
///
/// Each entry is a pointer previously returned by an allocation.  While the
/// block is idle in the cache its first 8 bytes are repurposed to hold the
/// address of the next cached entry; the bytes belong to the (unused) payload
/// region, so this is safe for the duration the block is cached.  Entries must
/// therefore be at least `MIN_ALLOC` (8) bytes and 8-aligned, which the
/// allocator guarantees.
struct PerCpuCache {
    head: *mut u8,
    count: usize,
}

// The pointer is only ever manipulated under the per-slot Mutex; the struct is
// never handed out by value across threads.
unsafe impl Send for PerCpuCache {}
unsafe impl Sync for PerCpuCache {}

impl PerCpuCache {
    const fn const_empty() -> Self {
        PerCpuCache {
            head: core::ptr::null_mut(),
            count: 0,
        }
    }

    fn len(&self) -> usize {
        self.count
    }

    fn push(&mut self, payload: *mut u8) {
        unsafe {
            *(payload as *mut *mut u8) = self.head;
        }
        self.head = payload;
        self.count += 1;
    }
}

/// Per-CPU free-block staging — `IrqMutex` so an IRQ handler freeing on the
/// same CPU while the interrupted thread holds its own cache cannot spin
/// forever. The cache link lives in payload bytes, so IRQ-time free is valid.
static PER_CPU_CACHE: [IrqMutex<PerCpuCache>; crate::smp::MAX_CPUS] = [
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
    IrqMutex::new(PerCpuCache::const_empty()),
];

/// Try to satisfy `layout` from the current CPU's private cache.
///
/// Scans the entire LIFO for a fitting block instead of probing only the head,
/// so a large alignment/size request does not starve while smaller cached blocks
/// could satisfy it. Costs O(CPU_CACHE_CAP) (64) in worst case.
fn per_cpu_alloc_cached(layout: Layout) -> *mut u8 {
    let cpu = crate::smp::current_cpu_id() as usize;
    let mut cache = PER_CPU_CACHE[cpu].lock();
    if cache.head.is_null() {
        return core::ptr::null_mut();
    }
    let align = layout.align().max(BLOCK_ALIGN);
    let need = layout.size();
    // Scan for first fitting entry, unlink it.
    let mut prev: *mut u8 = core::ptr::null_mut();
    let mut cur = cache.head;
    while !cur.is_null() {
        let next = unsafe { *(cur as *const *mut u8) };
        let header = unsafe { BlockHeader::from_payload(cur) };
        let usable = unsafe { (header as usize) + (*header).size } - (cur as usize);
        if usable >= need && (cur as usize) % align == 0 {
            // Unlink cur
            if prev.is_null() {
                cache.head = next;
            } else {
                unsafe { *(prev as *mut *mut u8) = next; }
            }
            cache.count -= 1;
            return cur;
        }
        prev = cur;
        cur = next;
    }
    core::ptr::null_mut()
}

/// Park `payload` in the current CPU's cache.
///
/// Returns `true` if it was cached, `false` if the cache is full and the block
/// should instead be returned to the shared arena.
fn per_cpu_free_cached(payload: *mut u8) -> bool {
    let cpu = crate::smp::current_cpu_id() as usize;
    let mut cache = PER_CPU_CACHE[cpu].lock();
    if cache.len() >= CPU_CACHE_CAP {
        // Cache full: route the block to the shared arena so chunk
        // reclamation still makes progress.
        return false;
    }
    cache.push(payload);
    true
}

pub struct HeapAllocator;

unsafe impl GlobalAlloc for HeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !HEAP_INITIALIZED.load(Ordering::SeqCst) {
            return core::ptr::null_mut();
        }

        // Fast path: reuse a block parked in this CPU's cache.
        let cached = per_cpu_alloc_cached(layout);
        if !cached.is_null() {
            // Live accounting only drives chunk reclamation, which is compiled
            // out when disabled.  Keeping this off the cache hit path means the
            // per-CPU cache truly never touches the global HEAP lock.
            if ENABLE_HEAP_CHUNK_RECLAIM {
                HEAP.lock().mark_live(cached as u64);
            }
            return cached;
        }

        let mut heap = HEAP.lock();
        let ptr = heap.alloc_inner(layout);
        let ptr = if ptr.is_null() {
            // Grow by at least the number of pages required for this
            // allocation so that a single large resize (e.g. a hashmap
            // backing array at ~260 KiB) does not loop indefinitely.
            // +1 page reserves room for the block header/padding so an
            // allocation that is an exact multiple of the page size (e.g. a
            // 3 MiB framebuffer shadow) still fits in the grown chunk.
            let pages_needed = (layout.size() + 4095) / 4096 + 1;
            allocate_pages(&mut heap, pages_needed.max(HEAP_GROW_PAGES));
            heap.alloc_inner(layout)
        } else {
            ptr
        };
        if !ptr.is_null() {
            if ENABLE_HEAP_CHUNK_RECLAIM {
                heap.mark_live(ptr as u64);
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        // Park the block in the current CPU's cache when there is room.  When
        // chunk reclamation is compiled out this is the entire cost of a free —
        // no global lock, no chunk scan.  When reclamation is enabled the
        // live-accounting is updated immediately (the block is idle even
        // though it lingers in a cache); freeing a chunk's last block (`live
        // == 0`) runs reclaim right away: its cached blocks are drained back
        // onto the free list, so the chunk body can collapse to one block.
        // This makes reclamation deterministic — one shot per transition —
        // instead of waiting for the next cache-overflow free (the old O(n²)
        // hot path).
        if per_cpu_free_cached(ptr) {
            if !ENABLE_HEAP_CHUNK_RECLAIM {
                return;
            }
            let mut records = [ReclaimRecord::ZERO; MAX_RECLAIMS_PER_CALL];
            let (count, root) = {
                let mut heap = HEAP.lock();
                let mut count = 0;
                if let Some((idx, live)) = heap.unmark_live(ptr as u64) {
                    if live == 0 && heap.chunk_is_reclaimable(idx) {
                        count = heap.try_reclaim(idx, &mut records);
                    }
                }
                (count, heap.root)
            };
            HeapInner::reclaim_unmapped(root, &records[..count]);
            return;
        }
        let mut records = [ReclaimRecord::ZERO; MAX_RECLAIMS_PER_CALL];
        let (count, root) = {
            let mut heap = HEAP.lock();
            let block = unsafe { BlockHeader::from_payload(ptr) };
            unsafe { (*block).next = core::ptr::null_mut() }
            heap.push_free(block);
            let mut count = 0;
            if ENABLE_HEAP_CHUNK_RECLAIM {
                if let Some((idx, live)) = heap.unmark_live(ptr as u64) {
                    if live == 0 && heap.chunk_is_reclaimable(idx) {
                        count = heap.try_reclaim(idx, &mut records);
                    }
                }
            }
            (count, heap.root)
        };
        HeapInner::reclaim_unmapped(root, &records[..count]);
    }
}

#[global_allocator]
static ALLOCATOR: HeapAllocator = HeapAllocator;
