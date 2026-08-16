//! BedrockOS `/dev/fb` display device.
//!
//! An immutable, `Copy` snapshot of the boot framebuffer's geometry and
//! mapped virtual address, plus lock-serialized bulk byte copies into that
//! framebuffer for the `/dev/fb` provider to drive.
//!
//! Modeled on the `audio` subsystem pattern: a `spin::Once` global is
//! populated exactly once at boot by [`register`].  The framebuffer is a
//! single-consumer, write-through device — the kernel draws through a shadow
//! buffer and flushes dirty rectangles, so the reads/writes here are the only
//! raw framebuffer access `fb` users make.  They address raw framebuffer
//! bytes in the native pixel format (rows of `stride * bpp` bytes, in
//! `pixel_format`), never premultiplied or color-converted.
//!
//! This module is `no_std`, panic-free, and never allocates: `read_at` copies
//! into the caller's slice and every failure mode returns `Option`/`bool`/
//! `usize` instead of panicking.

use spin::Once;

/// An immutable snapshot of the boot framebuffer.
///
/// `va` is the framebuffer's mapped virtual address (never a physical one);
/// the geometry fields describe the native surface `va` points at.  `size` is
/// the total framebuffer extent in bytes (`stride * height * bpp`).
#[derive(Clone, Copy, Debug)]
pub struct FramebufferDevice {
    pub va: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bpp: u8,
    pub pixel_format: common::types::PixelFormat,
    pub size: u64,
}

static FB: Once<FramebufferDevice> = Once::new();

/// Serializes reads/writes/clears of the framebuffer memory itself.
static COPY_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Store the boot framebuffer snapshot.
///
/// Returns `Some(())` the first time it runs and `None` on every later call
/// (idempotent).  Never panics: the `spin::Once` is only driven while
/// uninitialized, and the closure cannot fail.
pub fn register(fb: &framebuffer::Framebuffer) -> Option<()> {
    if FB.is_completed() {
        return None;
    }
    let dev = FramebufferDevice {
        va: fb.ptr() as u64,
        width: fb.width() as u32,
        height: fb.height() as u32,
        stride: fb.stride() as u32,
        bpp: fb.bpp(),
        pixel_format: fb.pixel_format(),
        size: fb.total_bytes() as u64,
    };
    FB.call_once(|| dev);
    Some(())
}

/// The registered framebuffer snapshot, if any.
pub fn get() -> Option<FramebufferDevice> {
    FB.get().copied()
}

/// Copy up to `buf.len()` raw framebuffer bytes starting at byte `offset` into
/// `buf`.  Clamped to the device size; returns the number of bytes copied, or
/// `0` when no framebuffer is registered (or the offset is past the end).
pub fn read_at(offset: u64, buf: &mut [u8]) -> usize {
    let Some(fb) = FB.get() else { return 0 };
    if buf.is_empty() || offset >= fb.size {
        return 0;
    }
    let available = fb.size - offset;
    let n = core::cmp::min(available, buf.len() as u64) as usize;
    let src = (fb.va as *mut u8).wrapping_add(offset as usize);
    let _guard = COPY_LOCK.lock();
    unsafe {
        core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), n);
    }
    n
}

/// Write raw framebuffer bytes at byte `offset`.  Returns `false` when no
/// framebuffer is registered or the write would overrun the device; otherwise
/// copies the whole slice and returns `true`.
pub fn write_at(offset: u64, bytes: &[u8]) -> bool {
    let Some(fb) = FB.get() else { return false };
    let Some(end) = offset.checked_add(bytes.len() as u64) else {
        return false;
    };
    if end > fb.size {
        return false;
    }
    if bytes.is_empty() {
        return true;
    }
    let dst = (fb.va as *mut u8).wrapping_add(offset as usize);
    let _guard = COPY_LOCK.lock();
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    }
    true
}

/// Zero the whole framebuffer.  Returns `false` when no framebuffer is
/// registered.
pub fn clear() -> bool {
    let Some(fb) = FB.get() else { return false };
    let _guard = COPY_LOCK.lock();
    unsafe {
        core::ptr::write_bytes(fb.va as *mut u8, 0, fb.size as usize);
    }
    true
}
