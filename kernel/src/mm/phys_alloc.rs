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

pub struct BitmapAllocator {
    bitmap: *mut u8,
    total_frames: usize,
    alloc_end: u64,
    next_free: usize,
}

impl BitmapAllocator {
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
    /// Find the highest physical address of any Usable memory region.
    fn find_max_addr(memory_map: &[MemoryRegion]) -> u64 {
        let mut max_addr = 0u64;
        for region in memory_map {
            if region.kind == MemoryRegionKind::Usable {
                let end = region.base.saturating_add(region.size);
                if end > max_addr { max_addr = end; }
            }
        }
        max_addr
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
            unsafe { *bitmap.add(0) |= 1; }
        }

        SerialPort::puts("[alloc] done\n");
        BitmapAllocator {
            bitmap,
            total_frames,
            alloc_end: max_addr,
            next_free: (base / 4096) as usize,
        }
    }

    /// Highest physical address managed by this allocator (exclusive).
    ///
    /// Used by virtual-memory setup to ensure all managed RAM is mapped.
    pub fn managed_end(&self) -> u64 {
        (self.total_frames as u64) * 4096
    }

    /// Highest address backed by *physical RAM* that this allocator can hand
    /// out (exclusive). This is the true end of usable memory, NOT the end of
    /// the address space (which can be terabytes due to huge MMIO/address-
    /// space holes). Page-table mapping must be bounded by this, otherwise we
    /// would try to fabricate page tables for address ranges that have no real
    /// RAM behind them.
    pub fn alloc_end(&self) -> u64 {
        self.alloc_end
    }

    /// Total number of 4 KiB frames this allocator can address.
    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// Allocate a physical frame.
    ///
    /// Returns physical address of allocated frame, or None if no frames available.
    pub fn alloc(&mut self) -> Option<u64> {
        // INV-PA-02: linear scan, find first free frame
        for i in self.next_free..self.total_frames {
            if self.is_free(i) {
                self.set_used(i);
                self.next_free = i + 1;
                return Some((i as u64) * 4096);
            }
        }
        None
    }

    /// Mark a physical address range as used (reserved).
    ///
    /// Used to prevent the allocator from handing out frames that contain
    /// critical data (kernel image, page tables, etc.).
    pub fn reserve_region(&mut self, start: u64, end: u64) {
        debug_assert!(start <= end, "reserve_region: start > end");
        let start_frame = (start / 4096) as usize;
        let end_frame = ((end + 4095) / 4096) as usize;
        for frame in start_frame..end_frame {
            if frame < self.total_frames {
                self.set_used(frame);
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
    pub unsafe fn free(&mut self, addr: u64) {
        let idx = (addr / 4096) as usize;
        if idx >= self.total_frames {
            return;
        }
        // INV-PA-03: clear bit
        self.set_free(idx);
        if idx < self.next_free {
            self.next_free = idx;
        }
    }

    fn is_free(&self, idx: usize) -> bool {
        unsafe { *self.bitmap.add(idx / 8) & (1 << (idx % 8)) == 0 }
    }

    fn set_used(&mut self, idx: usize) {
        unsafe { *self.bitmap.add(idx / 8) |= 1 << (idx % 8); }
    }

    fn set_free(&mut self, idx: usize) {
        unsafe { *self.bitmap.add(idx / 8) &= !(1 << (idx % 8)); }
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
