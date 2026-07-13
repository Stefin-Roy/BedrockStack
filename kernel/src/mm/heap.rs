use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;

const HEADER_SIZE: usize = size_of::<BlockHeader>();
const BLOCK_ALIGN: usize = core::mem::align_of::<BlockHeader>();
const BACKPTR_SIZE: usize = size_of::<*mut BlockHeader>();
const MIN_ALLOC: usize = 8;
const MIN_BLOCK_SIZE: usize = HEADER_SIZE + BACKPTR_SIZE + MIN_ALLOC;
const HEAP_INIT_PAGES: usize = 64;
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

pub struct HeapInner {
    free_list: *mut BlockHeader,
}

unsafe impl Send for HeapInner {}
unsafe impl Sync for HeapInner {}

impl HeapInner {
    pub const fn empty() -> Self {
        HeapInner {
            free_list: core::ptr::null_mut(),
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

    fn alloc_inner(&mut self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(BLOCK_ALIGN);
        let needed = layout.size().max(MIN_ALLOC);

        let mut prev: *mut BlockHeader = core::ptr::null_mut();
        let mut curr = self.free_list;

        while !curr.is_null() {
            let size = unsafe { (*curr).size };
            let next = unsafe { (*curr).next };
            let block_addr = curr as usize;
            let block_end = block_addr + size;
            let payload_addr = (block_addr + HEADER_SIZE + BACKPTR_SIZE + align - 1) & !(align - 1);
            let payload_end = payload_addr + needed;
            // Every free block begins with a `BlockHeader`, so the split
            // point must preserve that alignment even when `needed` is small.
            let alloc_end = (payload_end + BLOCK_ALIGN - 1) & !(BLOCK_ALIGN - 1);

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
static mut PHYS_ALLOCATOR: *mut BitmapAllocator = core::ptr::null_mut();

unsafe fn phys_allocator() -> &'static mut BitmapAllocator {
    let ptr = unsafe { PHYS_ALLOCATOR };
    if ptr.is_null() {
        SerialPort::puts("[heap] FATAL: no physical allocator for growth\n");
        loop {}
    }
    unsafe { &mut *ptr }
}

/// Initialise the kernel heap.
///
/// # Safety
/// Must be called exactly once, after the physical allocator is ready.
pub unsafe fn init(phys: &mut BitmapAllocator) {
    SerialPort::puts("[heap] init\n");

    unsafe { PHYS_ALLOCATOR = phys as *mut BitmapAllocator; }

    let mut heap = HEAP.lock();
    allocate_pages(&mut heap, HEAP_INIT_PAGES);
    HEAP_INITIALIZED.store(true, Ordering::SeqCst);
    SerialPort::puts("[heap] init done, pages=0x");
    SerialPort::put_hex(HEAP_INIT_PAGES as u64);
    SerialPort::puts(" total=");
    SerialPort::put_u64((HEAP_INIT_PAGES * 4096) as u64);
    SerialPort::puts(" bytes\n");
}

fn allocate_pages(heap: &mut HeapInner, count: usize) {
    let phys = unsafe { phys_allocator() };

    for _ in 0..count {
        if let Some(addr) = phys.alloc() {
            unsafe {
                heap.add_region(addr as usize, 4096);
            }
        } else {
            SerialPort::puts("[heap] WARN: out of physical frames\n");
            break;
        }
    }
}

pub struct HeapAllocator;

unsafe impl GlobalAlloc for HeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !HEAP_INITIALIZED.load(Ordering::SeqCst) {
            return core::ptr::null_mut();
        }

        let mut heap = HEAP.lock();
        let ptr = heap.alloc_inner(layout);
        if ptr.is_null() {
            allocate_pages(&mut heap, HEAP_GROW_PAGES);
            heap.alloc_inner(layout)
        } else {
            ptr
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let mut heap = HEAP.lock();
        let block = unsafe { BlockHeader::from_payload(ptr) };
        unsafe { (*block).next = core::ptr::null_mut() }
        heap.push_free(block);
    }
}

#[global_allocator]
static ALLOCATOR: HeapAllocator = HeapAllocator;
