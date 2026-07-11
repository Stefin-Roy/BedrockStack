//! Custom global allocator using OS_DATA memory type.
//!
//! All allocations use MemoryType 0x80000001 (OS_DATA), which persists
//! after exit_boot_services. This allows Vec allocations to survive
//! the UEFI boot services teardown.

use core::alloc::{GlobalAlloc, Layout};
use core::mem::size_of;
use uefi::boot;
use uefi::mem::memory_map::MemoryType;

/// Custom memory type for data that persists after exit_boot_services.
pub const OS_DATA: MemoryType = MemoryType::custom(0x80000001);

/// Alignment guaranteed by `allocate_pool`.
const POOL_ALIGN: usize = 8;

pub struct OsDataAllocator;

unsafe impl GlobalAlloc for OsDataAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();

        // Fast path: UEFI pools are always at least 8-byte aligned.
        if align <= POOL_ALIGN {
            return match unsafe { boot::allocate_pool(OS_DATA, layout.size()) } {
                Ok(ptr) => ptr.as_ptr(),
                Err(_) => core::ptr::null_mut(),
            };
        }

        let total = match layout
            .size()
            .checked_add(align)
            .and_then(|v| v.checked_add(size_of::<usize>()))
        {
            Some(v) => v,
            None => return core::ptr::null_mut(),
        };

        let base = match unsafe { boot::allocate_pool(OS_DATA, total) } {
            Ok(ptr) => ptr.as_ptr() as usize,
            Err(_) => return core::ptr::null_mut(),
        };

        let aligned = (base + size_of::<usize>() + align - 1) & !(align - 1);
        // Store the original pool pointer just below the aligned address.
        unsafe { *((aligned - size_of::<usize>()) as *mut usize) = base; }
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        let base = if layout.align() <= POOL_ALIGN {
            ptr as usize
        } else {
            unsafe { *((ptr as usize - size_of::<usize>()) as *const usize) }
        };

        if let Some(nn) = core::ptr::NonNull::new(base as *mut u8) {
            let _ = unsafe { boot::free_pool(nn) };
        }
    }
}
