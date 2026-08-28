//! Continuous `boot.log` sync to the ESP (`B>EFI/BEDROCK/boot.log`).
//!
//! The one-shot `dump_boot_log()` in `task::load` truncates the file at boot
//! with the early capture. This module keeps it live forever by appending
//! deltas every `SYNC_INTERVAL_NS`. It is deliberately called from the idle
//! loop (`Kernel::run`) rather than from a timer ISR, so VFS (which takes
//! `IrqMutex`/`NS_LOCK` and may block) is safe — the ISR never touches the
//! filesystem.
//!
//! State is three atomics: `FLUSHED_LEN` (bytes already on ESP),
//! `LAST_SYNC_NS` (throttle) and `FIRST_DONE` (first successful flush uses
//! `TRUNC` to overwrite the previous boot's file, later flushes use
//! `APPEND`). `mark_initial_flushed()` is called by the one-shot dump so
//! the periodic path knows where to resume.

#[cfg(target_arch = "x86_64")]
use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(target_arch = "x86_64")]
static FLUSHED_LEN: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static LAST_SYNC_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
static FIRST_DONE: AtomicBool = AtomicBool::new(false);

#[cfg(target_arch = "x86_64")]
const SYNC_INTERVAL_NS: u64 = 5_000_000_000; // 5 s

/// Called by the one-shot dump after it has successfully written `len` bytes
/// with `TRUNC`. Records the flushed position so the periodic appender resumes
/// at the right offset and does not re-truncate.
#[cfg(target_arch = "x86_64")]
pub fn mark_initial_flushed(len: usize) {
    FLUSHED_LEN.store(len as u64, Ordering::Release);
    FIRST_DONE.store(true, Ordering::Release);
    if crate::services::universal_timer::is_ready() {
        LAST_SYNC_NS.store(crate::services::universal_timer::now_ns(), Ordering::Relaxed);
    }
}

/// Idle-loop helper: if `SYNC_INTERVAL_NS` has elapsed and the capture log
/// has grown since the last flush, append the delta to
/// `B>EFI/BEDROCK/boot.log` and `sync_all`. Throttled, best-effort, never
/// panics — a missing ESP or VFS error simply leaves `FLUSHED_LEN` unchanged
/// so the next interval retries the same delta.
#[cfg(target_arch = "x86_64")]
pub fn maybe_sync() {
    if !crate::services::universal_timer::is_ready() {
        return;
    }
    let now = crate::services::universal_timer::now_ns();
    let last = LAST_SYNC_NS.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < SYNC_INTERVAL_NS {
        return;
    }

    // Snapshot capture without holding lock across VFS.
    let mut bytes = Vec::new();
    crate::drivers::serial::capture_bytes(&mut bytes);
    let flushed = FLUSHED_LEN.load(Ordering::Acquire) as usize;
    if bytes.len() <= flushed {
        // No new data — still throttle.
        LAST_SYNC_NS.store(now, Ordering::Relaxed);
        return;
    }
    let delta = &bytes[flushed..];
    let first = !FIRST_DONE.load(Ordering::Acquire);

    // IOMMU fault fallback mirrors dump_boot_log: if the display engine is
    // faulting, fall back to unprotected DMA before the storage flush.
    if crate::iommu::is_enabled() && crate::iommu::has_pending_faults() {
        crate::drivers::serial::SerialPort::puts("[bootlog] IOMMU faults -> fallback before sync\n");
        crate::iommu::fault_handler();
        crate::iommu::disable_all();
    }

    let ok = do_flush(delta, first);
    if ok {
        FLUSHED_LEN.store(bytes.len() as u64, Ordering::Release);
        FIRST_DONE.store(true, Ordering::Release);
        LAST_SYNC_NS.store(now, Ordering::Relaxed);
    } else {
        // Throttle retries even on failure to avoid spamming VFS.
        LAST_SYNC_NS.store(now, Ordering::Relaxed);
        // If we failed due to IOMMU faults, give the next interval a chance
        // after fallback; otherwise just retry next period.
    }
}

#[cfg(target_arch = "x86_64")]
fn do_flush(delta: &[u8], first: bool) -> bool {
    use crate::filesystems::vfs;
    use crate::filesystems::vfs::types::OpenFlags;

    let path = "B>EFI/BEDROCK/boot.log";
    let flags = if first {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNC
    } else {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND
    };

    let fd = match vfs::open(path, flags) {
        Ok(fd) => fd,
        Err(_) => {
            // Retry once after IOMMU fallback if still enabled.
            if crate::iommu::is_enabled() {
                crate::iommu::fault_handler();
                crate::iommu::disable_all();
                match vfs::open(path, flags) {
                    Ok(fd2) => fd2,
                    Err(_) => return false,
                }
            } else {
                return false;
            }
        }
    };

    let mut off = 0usize;
    let mut ok = true;
    while off < delta.len() {
        let chunk = &delta[off..core::cmp::min(off + 4096, delta.len())];
        match vfs::write(fd, chunk) {
            Ok(n) if n > 0 => off += n,
            Ok(_) => {
                ok = false;
                break;
            }
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    let _ = vfs::close(fd);
    let _ = vfs::sync_all();
    if ok && off != delta.len() {
        ok = false;
    }
    // On incomplete write, do not advance FLUSHED_LEN; next interval retries delta.
    // First flush that was truncated but only partially written is still considered
    // not done, so next attempt will re-truncate and retry the full delta.
    if !ok && first {
        // Leave FIRST_DONE false so next attempt re-truncates rather than appending a partial.
        FIRST_DONE.store(false, Ordering::Release);
    }
    ok
}

#[cfg(not(target_arch = "x86_64"))]
pub fn mark_initial_flushed(_len: usize) {}

#[cfg(not(target_arch = "x86_64"))]
pub fn maybe_sync() {}
