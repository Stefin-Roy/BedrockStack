pub mod plic;
pub mod htif;

use core::sync::atomic::{AtomicUsize, Ordering};

static DTB_PTR: AtomicUsize = AtomicUsize::new(0);

/// Store the DTB pointer (called from the boot entry point).
pub fn set_dtb_ptr(ptr: *const u8) {
    DTB_PTR.store(ptr as usize, Ordering::Relaxed);
}

/// Retrieve the DTB pointer, or null if not set.
pub fn get_dtb_ptr() -> Option<*const u8> {
    let val = DTB_PTR.load(Ordering::Relaxed);
    if val == 0 { None } else { Some(val as *const u8) }
}
