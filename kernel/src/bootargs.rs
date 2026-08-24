//! Formal kernel command line / bootargs.
//!
//! Single source of truth for Multiboot2 tag 1 (`boot command line`) and the
//! UEFI fallback. No heap before `init` – the raw string from the low
//! Multiboot2 info buffer (identity-mapped only before `switch_to_higher_half`)
//! is copied into a kernel-resident stash (`BOOTARGS_BUF`, like `RSDP_BUF`).
//! After that every consumer uses `get()` / `contains_word()`.
//!
//! Parsing is whitespace-split exact-word, no alloc, no case folding.
//! `nokaslr` is checked via `is_nokaslr()` (also accepts `-nokaslr` for
//! GRUB compatibility).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const BUF_SIZE: usize = 512;

static mut BOOTARGS_BUF: [u8; BUF_SIZE] = [0u8; BUF_SIZE];
static BOOTARGS_LEN: AtomicUsize = AtomicUsize::new(0);
static BOOTARGS_INIT: AtomicBool = AtomicBool::new(false);

/// Copy `raw` (already NUL-trimmed, may be non-UTF8) into the stash and
/// mark initialized. Called once from `rust_entry_mb2` while `info` is still
/// identity-mapped, and from `kernel_main` UEFI path as empty.
unsafe fn stash(raw: &[u8]) -> &'static str {
    let take = core::cmp::min(raw.len(), BUF_SIZE);
    unsafe {
        core::ptr::copy_nonoverlapping(
            raw.as_ptr(),
            core::ptr::addr_of_mut!(BOOTARGS_BUF) as *mut u8,
            take,
        );
        BOOTARGS_LEN.store(take, Ordering::Release);
        BOOTARGS_INIT.store(true, Ordering::Release);
        let s = core::slice::from_raw_parts(core::ptr::addr_of!(BOOTARGS_BUF) as *const u8, take);
        // Non-UTF8 bytes are lossy-replaced by trimming to valid prefix – keep simple: from_utf8 lossy fallback to empty on error.
        core::str::from_utf8(s).unwrap_or("")
    }
}

/// Initialize from a Multiboot2 cmdline slice (already trimmed at NUL).
/// Single-threaded BSP, pre-SMP. Safe to call multiple times – second is no-op.
pub unsafe fn init_from_slice(raw: &[u8]) {
    if BOOTARGS_INIT.load(Ordering::Acquire) {
        return;
    }
    unsafe { stash(raw); }
}

/// Initialize as empty (UEFI / RISC-V fallback). No cmdline delivered.
pub fn init_empty() {
    if BOOTARGS_INIT.swap(true, Ordering::SeqCst) {
        return;
    }
    BOOTARGS_LEN.store(0, Ordering::Release);
}

/// Initialize from a `'static` str (used by mb2 path after stash).
pub fn init_str(s: &'static str) {
    if BOOTARGS_INIT.load(Ordering::Acquire) {
        return;
    }
    let b = s.as_bytes();
    let take = core::cmp::min(b.len(), BUF_SIZE);
    unsafe {
        core::ptr::copy_nonoverlapping(
            b.as_ptr(),
            core::ptr::addr_of_mut!(BOOTARGS_BUF) as *mut u8,
            take,
        );
        BOOTARGS_LEN.store(take, Ordering::Release);
        BOOTARGS_INIT.store(true, Ordering::Release);
    }
}

/// Raw stashed bytes (may be empty). Returns empty slice before init.
fn as_bytes() -> &'static [u8] {
    let len = BOOTARGS_LEN.load(Ordering::Acquire);
    if len == 0 {
        return b"";
    }
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(BOOTARGS_BUF) as *const u8, len) }
}

/// Get the stashed cmdline as `&'static str`. `None` before any `init_*`, `Some("")` if empty.
pub fn get() -> Option<&'static str> {
    if !BOOTARGS_INIT.load(Ordering::Acquire) {
        return None;
    }
    let b = as_bytes();
    Some(core::str::from_utf8(b).unwrap_or(""))
}

/// Length of stashed cmdline (bytes).
pub fn len() -> usize {
    BOOTARGS_LEN.load(Ordering::Acquire)
}

/// True if initialized (even if empty).
pub fn is_init() -> bool {
    BOOTARGS_INIT.load(Ordering::Acquire)
}

/// Whitespace-split exact word match, no alloc. Matches `word` or `"-{word}"`.
pub fn contains_word(word: &str) -> bool {
    let b = as_bytes();
    if b.is_empty() || word.is_empty() {
        return false;
    }
    let w = word.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && b[i] != b' ' && b[i] != b'\t' && b[i] != b'\n' && b[i] != b'\r' {
            i += 1;
        }
        let token = &b[start..i];
        if token == w {
            return true;
        }
        if token.len() == w.len() + 1 && token[0] == b'-' && &token[1..] == w {
            return true;
        }
        if token.len() == w.len() + 2 && token[0] == b'-' && token[1] == b'-' && &token[2..] == w {
            return true;
        }
    }
    false
}

/// Convenience: `nokaslr` present (also `-nokaslr`).
pub fn is_nokaslr() -> bool {
    contains_word("nokaslr")
}

/// `noiommu` disables the VT-d IOMMU (also `-noiommu`). Without this flag
/// the IOMMU is always on when DMAR is present (opt-out, not opt-in).
pub fn is_noiommu() -> bool {
    contains_word("noiommu")
}
