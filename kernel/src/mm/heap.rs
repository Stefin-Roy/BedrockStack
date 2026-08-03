use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::drivers::serial::SerialPort;
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
    unsafe fn from_payload(ptr: *mut u8) -> *mut BlockHeader {
        unsafe { *((ptr as usize - BACKPTR_SIZE) as *const *mut BlockHeader) }
    }

    fn end(&self) -> usize {
        (self as *const Self as usize) + self.size
    }

    fn touches(&self, other: &BlockHeader) -> bool {
        self.end() == other as *const BlockHeader as usize
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
    const ZERO: Self = Self { vaddr: 0, phys: 0, size: 0, live: 0 };
}

/// Upper bound on the number of tracked heap growth regions.  The heap arena
/// is 512 MiB and the smallest growth a region can occupy (with its unmapped
/// guard page) is HEAP_GROW_PAGES + 1 pages (~64 KiB), so the arena can fit
/// ~8192 regions at most.  If this is ever exhausted we panic rather than
/// overflow a static array.
const MAX_CHUNKS: usize = 8192;

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
        unsafe { *block = BlockHeader { size, next: core::ptr::null_mut() } }
        self.push_free(block);
    }

    fn push_free(&mut self, block: *mut BlockHeader) {
        let block_ref = unsafe { &mut *block };

        // Try coalescing with head.
        if !self.free_list.is_null() {
            let head_ref = unsafe { &*self.free_list };
            if block_ref.touches(head_ref) {
                block_ref.size += head_ref.size;
                block_ref.next = head_ref.next;
                self.free_list = block;
                return;
            }
            // Check if head absorbs block.
            let block_end = block_ref.end();
            if self.free_list as usize == block_end {
                let head_ref = unsafe { &mut *self.free_list };
                head_ref.size += block_ref.size;
                return;
            }
        }

        block_ref.next = self.free_list;
        self.free_list = block;
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
        self.chunks[self.chunk_count] = ChunkMeta { vaddr, phys, size, live: 0 };
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
    fn unmark_live(&mut self, addr: u64) {
        if let Some(i) = self.chunk_idx(addr) {
            let c = &mut self.chunks[i];
            c.live = c.live.saturating_sub(1);
        }
    }

    /// True when the free list contains a single block header at `addr` whose
    /// size exactly spans `size` bytes (i.e. the body is one coalesced block).
    fn is_single_free_block(&self, addr: u64, size: u64) -> bool {
        let mut cur = self.free_list;
        while !cur.is_null() {
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
        while !cur.is_null() {
            if cur as usize == addr as usize {
                self.remove_next(prev);
                return true;
            }
            prev = cur;
            cur = unsafe { (*cur).next };
        }
        false
    }

    // ── Full-list coalescing ───────────────────────────────────────
    //
    // `push_free` only coalesces against the free-list head (O(1), but
    // ineffective for scattered free orders).  For chunk reclamation we need
    // the whole body of an idle chunk to collapse into ONE free block, so we
    // occasionally rebuild the free list into a fully-merged, address-sorted
    // form.  This runs only on the rare reclaim path (when a chunk is fully
    // idle), never on the hot alloc/free path.

    /// Rebuild the free list in-place, sorted by block start address
    /// (selection sort; O(n²), allocation-free — headers are just re-linked).
    fn sort_free_list(&mut self) {
        let mut new_head: *mut BlockHeader = core::ptr::null_mut();
        let mut new_tail: *mut BlockHeader = core::ptr::null_mut();

        while !self.free_list.is_null() {
            // Find the node with the lowest start address.
            let mut best_prev: *mut BlockHeader = core::ptr::null_mut();
            let mut best = self.free_list;
            let mut prev: *mut BlockHeader = core::ptr::null_mut();
            let mut cur = self.free_list;
            while !cur.is_null() {
                if (cur as usize) < (best as usize) {
                    best_prev = prev;
                    best = cur;
                }
                prev = cur;
                cur = unsafe { (*cur).next };
            }

            // Unlink `best`.
            if best_prev.is_null() {
                self.free_list = unsafe { (*best).next };
            } else {
                unsafe { (*best_prev).next = (*best).next; }
            }
            // Append to the rebuilt list.
            unsafe { (*best).next = core::ptr::null_mut(); }
            if new_tail.is_null() {
                new_head = best;
            } else {
                unsafe { (*new_tail).next = best; }
            }
            new_tail = best;
        }

        self.free_list = new_head;
    }

    /// Single pass over the (address-sorted) free list merging adjacent
    /// blocks.  Keeps the lower-address header as the merged block.
    fn merge_sorted_free_blocks(&mut self) {
        let mut cur = self.free_list;
        while !cur.is_null() {
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

    /// Best-effort reclaim of fully-idle growth regions.
    ///
    /// A chunk is reclaimed only when all its allocations are freed (`live ==
    /// 0`) and its whole body has coalesced into a single free block.  The
    /// lowest (base) chunk, which anchors the arena, is always reserved.
    /// Before scanning, the free list is fully coalesced so idle chunks can
    /// collapse into a single block regardless of the order blocks were freed.
    ///
    /// # Safety
    /// Must be called with the heap lock held and not re-entrant.
    fn try_reclaim(&mut self) {
        let base = self.low_vaddr;

        // Fast bail-out: nothing is idle, so skip the expensive coalesce.
        let has_idle = self.chunks[..self.chunk_count]
            .iter()
            .any(|c| c.live == 0 && c.vaddr != base);
        if !has_idle {
            return;
        }

        // Merge scattered free blocks so idle chunks form a single block.
        self.coalesce_all();

        let mut i = 0;
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
            self.remove_free_block(vaddr);

            let root = self.root;
            let pa = crate::mm::heap::get_phys_allocator_mut();
            let mut vmm = Vmm::from_root(root);
            // Unmap the body plus the trailing unmapped guard page.
            vmm.unmap(&mut *pa, vaddr, size + HEAP_GUARD_BYTES);
            // Return the contiguous physical frames.
            let end = phys.saturating_add(size);
            let mut f = phys;
            while f < end {
                unsafe { pa.free(f); }
                f += 4096;
            }

            SerialPort::puts("[heap] reclaim chunk vaddr=");
            SerialPort::put_hex(vaddr);
            SerialPort::puts(" phys=");
            SerialPort::put_hex(phys);
            SerialPort::puts(" size=");
            SerialPort::put_hex(size);
            SerialPort::puts(" live=0\n");

            // Shift trailing chunks down over the removed entry.
            self.chunks.copy_within(i + 1..self.chunk_count, i);
            self.chunk_count -= 1;
            // Do not advance `i`: the entry now at `i` is untested.
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
            let Some(block_end) = block_addr.checked_add(size) else { continue };
            let Some(payload_addr) = block_addr
                .checked_add(HEADER_SIZE + BACKPTR_SIZE + align - 1)
                .map(|v| v & !(align - 1))
            else { continue };
            let Some(payload_end) = payload_addr.checked_add(needed) else { continue };
            let Some(alloc_end) = payload_end
                .checked_add(BLOCK_ALIGN - 1)
                .map(|v| v & !(BLOCK_ALIGN - 1))
            else { continue };

            if alloc_end <= block_end {
                let remaining = block_end - alloc_end;

                if remaining >= MIN_BLOCK_SIZE {
                    // Split: allocate from the start of `curr`, replacing it
                    // in the free list with the remainder.  Do not use
                    // `push_free` here: it makes the remainder the head and
                    // `remove_next(prev)` would then remove that remainder,
                    // leaving the allocated block on the free list.
                    let alloc_size = alloc_end - block_addr;
                    unsafe { (*curr).size = alloc_size; }

                    let remainder_addr = block_addr + alloc_size;
                    let remainder = remainder_addr as *mut BlockHeader;
                    unsafe {
                        *remainder = BlockHeader { size: remaining, next };
                    }
                    if prev.is_null() {
                        self.free_list = remainder;
                    } else {
                        unsafe { (*prev).next = remainder; }
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
static HEAP: Mutex<HeapInner> = Mutex::new(HeapInner::empty());

/// Raw pointer to the physical allocator, stashed so `alloc()` can grow the heap.
///
/// # Safety
/// The caller must ensure the pointed-to `BitmapAllocator` outlives the
/// kernel heap (i.e. lives for the entire remaining boot sequence).  This
/// pointer is set once by `init()`, but may be *updated* after a move with
/// `set_phys_allocator()`.
static mut PHYS_ALLOCATOR: *mut BitmapAllocator = core::ptr::null_mut();

/// Update the physical allocator pointer after a move.
///
/// The heap stashes a raw pointer to the physical allocator during `init()`,
/// but the allocator struct may be moved (e.g. into `Kernel`).  Call this
/// once the final location is known so heap growth and DMA allocations
/// continue to work.
pub fn set_phys_allocator(phys: &mut BitmapAllocator) {
    unsafe { PHYS_ALLOCATOR = phys as *mut BitmapAllocator; }
}

/// Return a raw pointer to the physical allocator (may be null if uninitialised).
pub fn phys_allocator_raw() -> *mut BitmapAllocator {
    unsafe { PHYS_ALLOCATOR }
}

/// Return a mutable reference to the physical allocator.
///
/// # Panics
/// Panics if the allocator has not been initialised yet.
pub fn get_phys_allocator_mut() -> &'static mut BitmapAllocator {
    let ptr = unsafe { PHYS_ALLOCATOR };
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

    unsafe { PHYS_ALLOCATOR = phys as *mut BitmapAllocator; }

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

    let Some(pa) = phys.alloc_contiguous(count) else {
        SerialPort::puts("[heap] WARN: no contiguous frames for heap growth\n");
        return;
    };

    let size = (count as u64) * 4096;
    // Guard page separates this new chunk from the one below (or the top
    // of the arena for the first chunk).
    let low = if heap.low_vaddr == u64::MAX {
        HEAP_TOP.checked_sub(size).expect("heap: initial chunk exceeds arena")
    } else {
        heap.low_vaddr
            .checked_sub(HEAP_GUARD_BYTES)
            .and_then(|v| v.checked_sub(size))
            .expect("heap: chunk underflow below arena floor")
    };
    assert!(low >= HEAP_FLOOR, "heap: arena exhausted at {:#x}", low);

    let mut vmm = Vmm::from_root(heap.root);
    let alloc = &mut *phys;
    vmm.map(
        alloc,
        low,
        pa,
        size,
        PageFlags::READ | PageFlags::WRITE,
    );
    unsafe { heap.add_region(low as usize, size as usize); }
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
        PerCpuCache { head: core::ptr::null_mut(), count: 0 }
    }

    fn len(&self) -> usize {
        self.count
    }

    fn push(&mut self, payload: *mut u8) {
        unsafe { *(payload as *mut *mut u8) = self.head; }
        self.head = payload;
        self.count += 1;
    }

    fn pop(&mut self) -> *mut u8 {
        let p = self.head;
        if p.is_null() {
            return core::ptr::null_mut();
        }
        unsafe { self.head = *(p as *mut *mut u8); }
        self.count -= 1;
        p
    }
}

static PER_CPU_CACHE: [spin::Mutex<PerCpuCache>; crate::smp::MAX_CPUS] = [
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
    spin::Mutex::new(PerCpuCache::const_empty()),
];

/// Try to satisfy `layout` from the current CPU's private cache.
///
/// Returns a suitable payload, or `null` if the cache is empty or its head
/// block is too small/misaligned for `layout` (the block is left in the cache).
fn per_cpu_alloc_cached(layout: Layout) -> *mut u8 {
    let cpu = crate::smp::current_cpu_id() as usize;
    let mut cache = PER_CPU_CACHE[cpu].lock();
    let p = cache.pop();
    if p.is_null() {
        return core::ptr::null_mut();
    }
    // Reject candidates that cannot hold `layout`.  The back-pointer (kept at
    // `p - 8`, untouched while cached) recovers the block header whose `size`
    // is the usable extent of the freed payload.
    let header = unsafe { BlockHeader::from_payload(p) };
    let usable = unsafe { (header as usize) + (*header).size } - (p as usize);
    let align = layout.align().max(BLOCK_ALIGN);
    if usable >= layout.size() && (p as usize) % align == 0 {
        p
    } else {
        cache.push(p);
        core::ptr::null_mut()
    }
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
            HEAP.lock().mark_live(cached as u64);
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
            heap.mark_live(ptr as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        // Park the block in the current CPU's cache when there is room.  The
        // live-accounting for chunk reclamation is updated immediately (the
        // block is idle even though it lingers in a cache); `try_reclaim`
        // tolerates idle blocks hidden in caches because the chunk body can no
        // longer be a single global free block while they exist.
        if per_cpu_free_cached(ptr) {
            HEAP.lock().unmark_live(ptr as u64);
            return;
        }
        let mut heap = HEAP.lock();
        let block = unsafe { BlockHeader::from_payload(ptr) };
        unsafe { (*block).next = core::ptr::null_mut() }
        heap.push_free(block);
        heap.unmark_live(ptr as u64);
        heap.try_reclaim();
    }
}

#[global_allocator]
static ALLOCATOR: HeapAllocator = HeapAllocator;
