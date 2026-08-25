//! Physical frame allocator using a bitmap.
//!
//! Each bit represents one 4KB frame. 1 = used, 0 = free.
//!
//! # Invariants
//! - INV-PA-01: bitmap_len == (total_frames + 7) / 8
//! - INV-PA-02: alloc() returns frame where bit was 0, sets it to 1
//! - INV-PA-03: free() clears bit to 0
//! - INV-PA-04: Reserved frames (from memory map) are never allocated
//! - INV-PA-05: No double allocation (frame allocated to at most one owner)

use crate::boot::{MemoryRegion, MemoryRegionKind};
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    NoFrames,
    InvalidCount,
}

struct BitmapAllocatorInner {
    next_free: usize,
}

/// Upper bound on tracked usable regions.  The bitmap constructor asserts
/// this holds; firmware maps (UEFI/Multiboot) stay far below it.
pub const MAX_USABLE_REGIONS: usize = 32;

pub struct BitmapAllocator {
    bitmap: *mut u8,
    total_frames: usize,
    alloc_end: u64,
    kernel_start: u64,
    kernel_end: u64,
    /// Compact snapshot of the usable RAM spans `(base, size)` captured at
    /// construction.  The physmap builder maps exactly these spans so MMIO /
    /// ACPI / framebuffer holes below `alloc_end` never get writable aliases.
    usable_regions: [(u64, u64); MAX_USABLE_REGIONS],
    usable_len: usize,
    /// Free-frame count maintained under `inner` (every mutation happens with
    /// the lock held, so the value is exact between operations).  A lockless
    /// `Relaxed` read may briefly trail a concurrent alloc/free; consumers
    /// needing exactness take `inner` via [`Self::free_frames_exact`].
    free_count: AtomicUsize,
    /// Sharded scan-start hint — lockless `Relaxed` load for `alloc` fast-path.
    /// `inner.next_free` remains authoritative (holds lock); hint merely shards
    /// start_word so concurrent allocators don't all probe word 0. No buddy yet.
    next_free_hint: AtomicUsize,
    inner: spin::Mutex<BitmapAllocatorInner>,
}

unsafe impl Send for BitmapAllocator {}
unsafe impl Sync for BitmapAllocator {}

impl BitmapAllocator {
    /// Translate the stored (physical) bitmap base through the current physmap
    /// offset, mirroring the VMM walkers.
    ///
    /// Before `init_physmap` the offset is 0 (identity); once the DIRECT_MAP at
    /// `PHYS_MAP_BASE` is live, low physical pages are only reachable through
    /// the physmap, so the bitmap must be deref'd at `to_physmap(base)`.  The
    /// bitmap lives in a usable region below 4 GiB, which Phase 4's tables map
    /// only at `PHYS_MAP_BASE + phys` — the old identity-window access would
    /// page-fault on the first post-switch allocation.
    fn bitmap_ptr(&self) -> *mut u8 {
        crate::mm::layout::to_physmap(self.bitmap as u64) as *mut u8
    }

    /// Create a new allocator.
    ///
    /// The bitmap is placed at the start of `bitmap_region`, unless that would
    /// overlap the kernel image `[kernel_start, kernel_end)`, in which case it
    /// is moved to just after the kernel (still within `bitmap_region`).
    ///
    /// All frames start as "used". Only frames within Usable memory regions
    /// are cleared to "free", so the allocator can never hand out frames
    /// that belong to MMIO devices, firmware, or non-existent memory.
    ///
    /// # Safety
    /// - bitmap_region is a valid (base, size) pair within a Usable region
    /// - memory_map is valid and describes physical memory
    fn find_max_addr(memory_map: &[MemoryRegion]) -> u64 {
        memory_map
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .map(|r| r.base.saturating_add(r.size))
            .max()
            .unwrap_or(0)
    }

    pub unsafe fn new(
        bitmap_region: (u64, u64),
        memory_map: &[MemoryRegion],
        kernel_start: u64,
        kernel_end: u64,
    ) -> Self {
        use crate::drivers::serial::SerialPort;
        let (region_base, region_size) = bitmap_region;
        assert!(region_size > 0, "no usable memory region for bitmap");
        let region_end = region_base + region_size;

        for r in memory_map {
            SerialPort::puts("[mmap] base=");
            SerialPort::put_hex(r.base);
            SerialPort::puts(" size=");
            SerialPort::put_hex(r.size);
            SerialPort::puts(" end=");
            SerialPort::put_hex(r.base.saturating_add(r.size));
            SerialPort::puts(" kind=");
            SerialPort::put_u64(r.kind as u64);
            SerialPort::puts("\n");
        }

        let max_addr = Self::find_max_addr(memory_map);
        let total_frames = (max_addr as usize + 4095) / 4096;
        let bitmap_len = (total_frames + 7) / 8;

        SerialPort::puts("[alloc] max_addr=");
        SerialPort::put_hex(max_addr);
        SerialPort::puts(" frames=");
        SerialPort::put_u64(total_frames as u64);
        SerialPort::puts(" bitmap_len=");
        SerialPort::put_u64(bitmap_len as u64);
        SerialPort::puts("\n");

        let base = if region_base < kernel_end {
            (kernel_end + 4095) & !4095
        } else {
            region_base
        };

        assert!(
            base >= region_base && base + bitmap_len as u64 <= region_end,
            "bitmap does not fit in usable region"
        );

        SerialPort::puts("[alloc] base=");
        SerialPort::put_hex(base);
        SerialPort::puts(" region_end=");
        SerialPort::put_hex(region_end);
        SerialPort::puts("\n");

        let bitmap = base as *mut u8;
        unsafe { core::ptr::write_bytes(bitmap, 0xFF, bitmap_len) };

        for region in memory_map {
            if region.kind == MemoryRegionKind::Usable {
                clear_region(bitmap, region, total_frames);
            }
        }

        mark_region_used(
            bitmap,
            &MemoryRegion {
                base,
                size: bitmap_len as u64,
                kind: MemoryRegionKind::Reserved,
            },
            total_frames,
        );

        if 0 < total_frames {
            unsafe {
                *bitmap.add(0) |= 1;
            }
        }

        // Runtime reserve: kernel image + frame 0 already handled, but enforce
        // hard reservation here regardless of external caller. This survives
        // release builds and protects against a malformed mmap where usable
        // overlaps kernel.
        let ks = kernel_start & !0xFFF;
        let ke = (kernel_end + 0xFFF) & !0xFFF;
        if ke > ks {
            mark_region_used(
                bitmap,
                &MemoryRegion {
                    base: ks,
                    size: ke - ks,
                    kind: MemoryRegionKind::Reserved,
                },
                total_frames,
            );
        }

        SerialPort::puts("[alloc] done\n");
        let hint = (base / 4096) as usize;
        let mut usable_regions = [(0u64, 0u64); MAX_USABLE_REGIONS];
        let mut usable_len = 0usize;
        for r in memory_map {
            if r.kind == MemoryRegionKind::Usable && r.size > 0 {
                assert!(
                    usable_len < MAX_USABLE_REGIONS,
                    "too many usable memory regions (>{MAX_USABLE_REGIONS})"
                );
                usable_regions[usable_len] = (r.base, r.size);
                usable_len += 1;
            }
        }
        BitmapAllocator {
            bitmap,
            total_frames,
            alloc_end: max_addr,
            kernel_start: ks,
            kernel_end: ke,
            usable_regions,
            usable_len,
            free_count: AtomicUsize::new(0),
            next_free_hint: AtomicUsize::new(hint),
            inner: spin::Mutex::new(BitmapAllocatorInner { next_free: hint }),
        }
        .with_seeded_free_count()
    }

    /// Seed `free_count` from a one-time full bitmap scan (boot only).
    fn with_seeded_free_count(mut self) -> Self {
        let guard = self.inner.lock();
        self.free_count = AtomicUsize::new(self.count_free_locked(&guard));
        drop(guard);
        self
    }

    /// The usable RAM spans `(base, size)` captured from the firmware memory
    /// map at construction.  The physmap builder maps exactly these spans —
    /// never the holes between them.
    pub fn usable_regions(&self) -> &[(u64, u64)] {
        &self.usable_regions[..self.usable_len]
    }

    /// Highest physical address of any usable region (exclusive).
    ///
    /// This is the top of the last usable RAM chunk, NOT the end of a contiguous
    /// block.  It bounds `init_physmap`'s window size; the *mapped* spans are
    /// exactly [`BitmapAllocator::usable_regions`], so MMIO/firmware holes below
    /// this bound get no writable alias.
    pub fn alloc_end(&self) -> u64 {
        self.alloc_end
    }

    /// Total number of 4 KiB frames this allocator can address.
    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// The allocator's forward-scan cursor (next frame index `alloc()` /
    /// `alloc_contiguous()` will start probing). Read-only; `dma_trace` uses it
    /// to report allocator state on a frame-alloc failure.
    pub fn next_free(&self) -> usize {
        self.next_free_hint.load(Ordering::Relaxed)
    }

    pub fn next_free_locked(&self) -> usize {
        self.inner.lock().next_free
    }

    /// Count of currently-free 4 KiB frames — O(1) atomic read.
    ///
    /// Maintained under `inner` on every alloc/free/reserve path, so the value
    /// is exact whenever no allocation is mid-flight; the lockless `Relaxed`
    /// read here may briefly trail concurrent mutations.  Use
    /// [`Self::free_frames_exact`] for an authoritative bitmap scan.
    pub fn free_frames(&self) -> usize {
        self.free_count.load(Ordering::Relaxed)
    }

    /// Authoritative free-frame count via full O(n) bitmap scan under `inner`.
    pub fn free_frames_exact(&self) -> usize {
        let guard = self.inner.lock();
        self.count_free_locked(&guard)
    }

    /// Full bitmap word scan; caller must hold `inner`.
    fn count_free_locked(&self, _guard: &spin::MutexGuard<'_, BitmapAllocatorInner>) -> usize {
        let total_words = (self.total_frames + 63) / 64;
        let bitmap_u64 = self.bitmap_ptr() as *const u64;
        let mut free = 0usize;
        for wi in 0..total_words {
            let mask = if wi == total_words - 1 {
                let rem = self.total_frames % 64;
                if rem == 0 {
                    u64::MAX
                } else {
                    (1u64 << rem) - 1
                }
            } else {
                u64::MAX
            };
            let w = unsafe { *bitmap_u64.add(wi) } & mask;
            free += mask.count_ones() as usize - w.count_ones() as usize;
        }
        free
    }

    /// Bit test without mutation; caller must hold `inner` where the bit may
    /// race (boot-time / locked paths only).
    fn is_used(&self, idx: usize) -> bool {
        unsafe { (*self.bitmap_ptr().add(idx / 8) >> (idx % 8)) & 1 == 1 }
    }

    #[inline]
    fn is_reserved_frame(&self, idx: usize) -> bool {
        if idx == 0 {
            return true;
        }
        let addr = (idx as u64) * 4096;
        addr >= self.kernel_start && addr < self.kernel_end
    }

    #[inline]
    fn range_overlaps_kernel(&self, start_idx: usize, count: usize) -> bool {
        if start_idx == 0 {
            return true;
        }
        let addr = (start_idx as u64) * 4096;
        let end = addr + (count as u64) * 4096;
        end > self.kernel_start && addr < self.kernel_end
    }

    /// Allocate a physical frame.
    ///
    /// Returns physical address of allocated frame, or None if no frames available.
    /// Runtime-enforces reservation of kernel image and frame 0 (not just debug_assert).
    pub fn alloc(&self) -> Option<u64> {
        let mut inner = self.inner.lock();
        // Advisory hint vs authoritative cursor: max avoids stale hint causing missed tail.
        let start_idx = self.next_free_hint.load(Ordering::Relaxed).max(inner.next_free);
        let total_words = (self.total_frames + 63) / 64;
        let bitmap_u64 = self.bitmap_ptr() as *const u64;

        let start_word = start_idx / 64;
        let start_bit = start_idx % 64;

        // First pass: start_word .. total_words
        for wi in start_word..total_words {
            let w = unsafe { *bitmap_u64.add(wi) };
            let mut candidates = !w
                & if wi == start_word && start_bit > 0 {
                    !((1u64 << start_bit) - 1)
                } else {
                    !0u64
                }
                & if wi == total_words - 1 && self.total_frames % 64 != 0 {
                    (1u64 << (self.total_frames % 64)) - 1
                } else {
                    !0u64
                };
            while candidates != 0 {
                let bit = candidates.trailing_zeros() as usize;
                let idx = wi * 64 + bit;
                if self.is_reserved_frame(idx) {
                    // Hard reserve — never hand out, mask and keep scanning same word.
                    candidates &= !(1u64 << bit);
                    continue;
                }
                self.set_used(idx);
                inner.next_free = idx + 1;
                self.next_free_hint.store(idx + 1, Ordering::Relaxed);
                let addr = (idx as u64) * 4096;
                debug_assert!(
                    addr < self.kernel_start || addr >= self.kernel_end,
                    "alloc: frame {:#x} is within kernel image [{:#x}, {:#x})",
                    addr,
                    self.kernel_start,
                    self.kernel_end
                );
                if addr >= self.kernel_start && addr < self.kernel_end {
                    // Runtime reserve enforcement (release builds)
                    self.set_free(idx);
                    candidates &= !(1u64 << bit);
                    continue;
                }
                self.free_count.fetch_sub(1, Ordering::Relaxed);
                return Some(addr);
            }
        }

        // Wrap-around: scan from 0 to start_word
        for wi in 0..start_word {
            let w = unsafe { *bitmap_u64.add(wi) };
            let mut candidates = !w
                & if wi == total_words - 1 && self.total_frames % 64 != 0 {
                    (1u64 << (self.total_frames % 64)) - 1
                } else {
                    !0u64
                };
            while candidates != 0 {
                let bit = candidates.trailing_zeros() as usize;
                let idx = wi * 64 + bit;
                if self.is_reserved_frame(idx) {
                    candidates &= !(1u64 << bit);
                    continue;
                }
                self.set_used(idx);
                inner.next_free = idx + 1;
                self.next_free_hint.store(idx + 1, Ordering::Relaxed);
                let addr = (idx as u64) * 4096;
                debug_assert!(
                    addr < self.kernel_start || addr >= self.kernel_end,
                    "alloc: frame {:#x} is within kernel image [{:#x}, {:#x})",
                    addr,
                    self.kernel_start,
                    self.kernel_end
                );
                if addr >= self.kernel_start && addr < self.kernel_end {
                    self.set_free(idx);
                    candidates &= !(1u64 << bit);
                    continue;
                }
                self.free_count.fetch_sub(1, Ordering::Relaxed);
                return Some(addr);
            }
        }
        None
    }

    /// Allocate `count` contiguous physical frames.
    ///
    /// Returns the physical address of the first frame, or `None` if
    /// insufficient contiguous frames are available.
    /// Runtime-enforces kernel + frame 0 reservation (never returns overlapping range).
    pub fn alloc_contiguous(&self, count: usize) -> Option<u64> {
        // Fallible alias kept for callers that used expect; new code prefers try_alloc_contiguous
        self.try_alloc_contiguous(count).ok()
    }

    /// Fallible contiguous allocation — returns Err(NoFrames) instead of panicking.
    /// Used by smp::allocate_ap_stack and heap::allocate_pages.
    pub fn try_alloc_contiguous(&self, count: usize) -> Result<u64, AllocError> {
        self.alloc_contig_core(count, 1, self.total_frames)
    }

    /// Fallible contiguous allocation whose entire span lies strictly below
    /// `max_paddr` — the 32-bit DMA-zone query.  Devices that cannot address
    /// above 4 GiB (e.g. HDA controllers without the 64-bit cap) allocate
    /// through this instead of failing init.
    pub fn try_alloc_contiguous_below(
        &self,
        count: usize,
        max_paddr: u64,
    ) -> Result<u64, AllocError> {
        let hi_cap = ((max_paddr / 4096) as usize).min(self.total_frames);
        if hi_cap == 0 || count > hi_cap {
            return Err(AllocError::NoFrames);
        }
        self.alloc_contig_core(count, 1, hi_cap)
    }

    /// Fallible contiguous allocation with an alignment requirement expressed
    /// in *frames* (`align_pages` must be a power of two; 1 = any frame).
    ///
    /// The returned base satisfies `base % (align_pages * 4096) == 0`.  Used
    /// for huge-page-backed consumers (e.g. heap growth wanting 2 MiB chunks
    /// → `align_pages = 512`) so `Vmm::map` can use large pages instead of
    /// degrading to 512 × 4 KiB PTEs.
    pub fn try_alloc_contiguous_aligned(
        &self,
        count: usize,
        align_pages: usize,
    ) -> Result<u64, AllocError> {
        self.alloc_contig_core(count, align_pages, self.total_frames)
    }

    /// Shared contiguous-run search: `[0, hi_cap)` frame window, `align_pages`
    /// alignment, wrap-around once.
    fn alloc_contig_core(
        &self,
        count: usize,
        align_pages: usize,
        hi_cap: usize,
    ) -> Result<u64, AllocError> {
        if count == 0 || count > self.total_frames || count > hi_cap {
            return Err(AllocError::InvalidCount);
        }
        if align_pages == 0 || !align_pages.is_power_of_two() {
            return Err(AllocError::InvalidCount);
        }
        let mut inner = self.inner.lock();
        let hint = self.next_free_hint.load(Ordering::Relaxed);
        let next_free = hint.max(inner.next_free);
        let total_words = (self.total_frames + 63) / 64;
        let bitmap_u64 = self.bitmap_ptr() as *const u64;
        let last_bits = if self.total_frames % 64 == 0 {
            64
        } else {
            self.total_frames % 64
        };

        // Find a run of `count` consecutive free frames within [lo, hi),
        // returning an `align_pages`-aligned start inside the run when one
        // fits.  Scans 64 frames per word, skipping fully-free words in one
        // shot instead of probing one frame at a time.
        //
        // `emit` picks the first aligned window inside a candidate run; when
        // alignment doesn't fit *yet* the caller keeps extending the run
        // rather than discarding it (the deficit is < align_pages frames).
        let emit = |rs: usize, rl: usize| -> Option<usize> {
            if rl < count {
                return None;
            }
            let off = (align_pages - (rs % align_pages)) % align_pages;
            if off + count <= rl {
                Some(rs + off)
            } else {
                None
            }
        };
        let find_run = |lo: usize, hi: usize| -> Option<usize> {
            if lo >= hi {
                return None;
            }
            let start_word = lo / 64;
            let end_word = hi / 64 + if hi % 64 == 0 { 0 } else { 1 };
            let mut run_start = 0usize;
            let mut run_len = 0usize;
            for wi in start_word..end_word {
                let nbits = if wi == total_words - 1 { last_bits } else { 64 };
                let word_mask = if nbits == 64 {
                    u64::MAX
                } else {
                    (1u64 << nbits) - 1
                };
                let mut w = unsafe { *bitmap_u64.add(wi) } & word_mask;
                // Enforce the [lo, hi) window: treat out-of-window bits as used.
                if wi == start_word {
                    let skip = lo % 64;
                    if skip != 0 {
                        w |= (1u64 << skip) - 1;
                    }
                }
                if wi == end_word - 1 {
                    let keep = hi % 64;
                    if keep != 0 {
                        w |= !((1u64 << keep) - 1) & word_mask;
                    }
                }

                if w == 0 {
                    // Whole word free: extend the run across all nbits.
                    if run_len == 0 {
                        run_start = wi * 64;
                    }
                    run_len += nbits;
                    if let Some(s) = emit(run_start, run_len) {
                        return Some(s);
                    }
                    continue;
                }

                // A used bit exists in this word.  Extend any carryover run
                // across the leading free bits, then look for fresh runs after
                // the break (never re-scan the already-consumed prefix).
                let mut i = 0usize;
                if run_len > 0 {
                    let lead = (!w).trailing_zeros() as usize;
                    run_len += lead;
                    if let Some(s) = emit(run_start, run_len) {
                        return Some(s);
                    }
                    run_len = 0;
                    i = lead + 1;
                }
                while i < nbits {
                    if w & (1u64 << i) == 0 {
                        let s = i;
                        while i < nbits && w & (1u64 << i) == 0 {
                            i += 1;
                        }
                        let len = i - s;
                        if let Some(a) = emit(wi * 64 + s, len) {
                            return Some(a);
                        }
                        run_len = len;
                        run_start = wi * 64 + s;
                    } else {
                        i += 1;
                    }
                }
            }
            None
        };

        // Scan from next_free to end-of-bitmap, then wrap around from 0.
        // Runtime reserve: skip runs overlapping kernel or frame 0 by re-probing.
        let mut lo = next_free;
        // `hi_cap` is an exclusive frame bound.  Using total_frames here
        // would let try_alloc_contiguous_below() return a span above its
        // advertised DMA ceiling whenever the low zone was fragmented.
        let mut hi = hi_cap;
        let mut tried_wrap = false;
        loop {
            if let Some(run_start) = find_run(lo, hi) {
                if self.range_overlaps_kernel(run_start, count) {
                    // treat as occupied, skip past it and continue search in same half
                    lo = run_start + 1;
                    if lo + count > hi {
                        if !tried_wrap && hi == hi_cap {
                            lo = 0;
                            hi = next_free;
                            tried_wrap = true;
                            continue;
                        } else {
                            break;
                        }
                    }
                    continue;
                }
                for j in run_start..run_start + count {
                    self.set_used(j);
                }
                inner.next_free = run_start + count;
                self.next_free_hint.store(run_start + count, Ordering::Relaxed);
                let addr = (run_start as u64) * 4096;
                let end_addr = addr + (count as u64) * 4096;
                debug_assert!(
                    end_addr <= self.kernel_start || addr >= self.kernel_end,
                    "alloc_contiguous: range [{:#x}, {:#x}) overlaps kernel [{:#x}, {:#x})",
                    addr,
                    end_addr,
                    self.kernel_start,
                    self.kernel_end
                );
                if end_addr > self.kernel_start && addr < self.kernel_end {
                    // Runtime reserve enforcement
                    for j in run_start..run_start + count {
                        self.set_free(j);
                    }
                    lo = run_start + 1;
                    continue;
                }
                self.free_count.fetch_sub(count, Ordering::Relaxed);
                return Ok(addr);
            } else {
                if !tried_wrap && hi == hi_cap {
                    lo = 0;
                    hi = next_free;
                    tried_wrap = true;
                    continue;
                }
                break;
            }
        }
        Err(AllocError::NoFrames)
    }

    /// Non-fallible wrapper for legacy callers; prefer try_alloc_contiguous.
    pub fn alloc_contiguous_checked(&self, count: usize) -> Option<u64> {
        self.try_alloc_contiguous(count).ok()
    }

    /// Mark a physical address range as used (reserved).
    ///
    /// Used to prevent the allocator from handing out frames that contain
    /// critical data (kernel image, page tables, etc.).
    pub fn reserve_region(&self, start: u64, end: u64) {
        let _guard = self.inner.lock();
        debug_assert!(start <= end, "reserve_region: start > end");
        let start_frame = (start / 4096) as usize;
        let end_frame = if end == u64::MAX {
            self.total_frames
        } else {
            ((end + 4095) / 4096).min(self.total_frames as u64) as usize
        };
        for frame in start_frame..end_frame {
            if frame < self.total_frames && !self.is_used(frame) {
                self.set_used(frame);
                self.free_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Reserve a physical range given as (addr, size). Frames outside the
    /// allocator's coverage are ignored (the caller still owns them).
    pub fn reserve_range(&mut self, addr: u64, size: u64) {
        self.reserve_region(addr, addr.saturating_add(size));
    }

    /// Free a physical frame.
    ///
    /// # Safety
    /// - addr must be a frame previously allocated by this allocator
    /// - addr must not be in use by any other component
    pub unsafe fn free(&self, addr: u64) {
        let mut inner = self.inner.lock();
        let idx = (addr / 4096) as usize;
        if idx >= self.total_frames {
            return;
        }
        // Never free reserved frames via runtime guard (warn in debug).
        if idx == 0 || ( (idx as u64)*4096 >= self.kernel_start && (idx as u64)*4096 < self.kernel_end) {
            debug_assert!(false, "phys_alloc::free: attempt to free reserved frame {:#x}", addr);
            crate::drivers::serial::SerialPort::puts("[alloc] WARN: free reserved frame ");
            crate::drivers::serial::SerialPort::put_hex(addr);
            crate::drivers::serial::SerialPort::puts("\n");
            return;
        }
        // INV-PA-03: clear bit
        self.set_free(idx);
        self.free_count.fetch_add(1, Ordering::Relaxed);
        if idx < inner.next_free {
            inner.next_free = idx;
        }
        // Shard hint backwards if this is earlier.
        let cur = self.next_free_hint.load(Ordering::Relaxed);
        if idx < cur {
            // Best-effort CAS loop; Relaxed ok for hint.
            let mut current = cur;
            while idx < current {
                match self.next_free_hint.compare_exchange_weak(current, idx, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(v) => current = v,
                }
            }
        }
    }

    fn set_used(&self, idx: usize) {
        unsafe {
            *self.bitmap_ptr().add(idx / 8) |= 1 << (idx % 8);
        }
    }

    fn set_free(&self, idx: usize) {
        unsafe {
            *self.bitmap_ptr().add(idx / 8) &= !(1 << (idx % 8));
        }
    }
}

/// Mark a memory region as free in the bitmap (clear bits).
///
/// `total_frames` bounds the write so a region reported above managed RAM
/// can never write past the end of the bitmap.
fn clear_region(bitmap: *mut u8, region: &MemoryRegion, total_frames: usize) {
    let start_frame = (region.base / 4096) as usize;
    let end = region.base.saturating_add(region.size);
    let end_frame = if end == u64::MAX {
        total_frames
    } else {
        ((end + 4095) / 4096).min(total_frames as u64) as usize
    };

    for frame in start_frame..end_frame {
        unsafe {
            *bitmap.add(frame / 8) &= !(1 << (frame % 8));
        }
    }
}

// ── Service provider traits ───────────────────────────────────────────
use crate::services::phys_mem::PhysicalMemoryAllocator;

impl PhysicalMemoryAllocator for BitmapAllocator {
    fn alloc_frames(&mut self, count: usize) -> Result<u64, ()> {
        if count == 1 {
            self.alloc().ok_or(())
        } else {
            self.alloc_contiguous(count).ok_or(())
        }
    }

    fn free_frames(&mut self, addr: u64, count: usize) {
        // `count` was previously ignored, leaking N-1 frames on every
        // `alloc_contiguous(N)` free. Iterate per-frame so the bitmap and
        // `next_free` stay consistent. Caller guarantees `addr` page-aligned.
        if count == 0 {
            return;
        }
        debug_assert!(addr % 4096 == 0, "free_frames: unaligned addr {:#x}", addr);
        for i in 0..count {
            let frame = addr + (i as u64) * 4096;
            unsafe {
                self.free(frame);
            }
        }
    }

    fn reserve_region(&mut self, start: u64, end: u64) {
        Self::reserve_region(self, start, end);
    }

    fn total_frames(&self) -> usize {
        self.total_frames()
    }
}

/// Mark a memory region as used in the bitmap.
///
/// `total_frames` bounds the write so a region reported above managed RAM
/// (e.g. high MMIO) can never write past the end of the bitmap.
fn mark_region_used(bitmap: *mut u8, region: &MemoryRegion, total_frames: usize) {
    let start_frame = (region.base / 4096) as usize;
    let end = region.base.saturating_add(region.size);
    // Avoid overflow when adding 4095 to u64::MAX (saturated). If end is
    // saturated to MAX, cap at total_frames directly.
    let end_frame = if end == u64::MAX {
        total_frames
    } else {
        ((end + 4095) / 4096).min(total_frames as u64) as usize
    };

    for frame in start_frame..end_frame {
        unsafe {
            *bitmap.add(frame / 8) |= 1 << (frame % 8);
        }
    }
}
