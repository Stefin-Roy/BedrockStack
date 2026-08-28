//! Lock-free kernel stage tracker for dumps.
//!
//! Updated at each major `Kernel::init` / `Kernel::run` step with a
//! `Relaxed` store so `#PF` / `#GP` / `panic` / `NMI` handlers can read it
//! without locks or heap. The on-screen panic screen (`screen.rs`) and the
//! serial dump (`dump.rs`) both read `as_str()`.

use core::sync::atomic::{AtomicU8, Ordering};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Early = 0,
    Heap,
    Acpi,
    Framebuffer,
    IoApic,
    Services,
    Smp,
    Watchdog,
    Pci,
    Input,
    Storage,
    Usb,
    Audio,
    Vfs,
    Namespace,
    Scheduler,
    Launch,
    Running,
}

static STAGE: AtomicU8 = AtomicU8::new(Stage::Early as u8);

/// Set the current kernel stage (lock-free, `Relaxed`).
#[inline]
pub fn set(s: Stage) {
    STAGE.store(s as u8, Ordering::Relaxed);
}

/// Raw variant for bootanim sync (no enum construction).
#[inline]
pub fn set_raw(v: u8) {
    STAGE.store(v, Ordering::Relaxed);
}

#[inline]
pub fn get_raw() -> u8 {
    STAGE.load(Ordering::Relaxed)
}

/// Human-readable stage string — matches `bootanim::STAGE_TEXTS` for the
/// shared prefix so the serial + screen banners agree.
pub fn as_str() -> &'static str {
    match get_raw() {
        0 => "early",
        1 => "heap init",
        2 => "acpi init",
        3 => "framebuffer shadow",
        4 => "ioapic init",
        5 => "services init",
        6 => "smp init",
        7 => "watchdog init",
        8 => "pci init",        // bootanim::Stage::Pci
        9 => "input init",      // Stage::Input
        10 => "storage init",   // Stage::Storage
        11 => "usb init",       // Stage::Usb
        12 => "audio init",     // Stage::Audio
        13 => "vfs init",       // Stage::Vfs
        14 => "namespace init", // Stage::Namespace
        15 => "scheduler init", // Stage::Scheduler
        16 => "launch",         // Stage::Launch
        17 => "running (idle)", // post-launch idle loop
        _ => "unknown",
    }
}

/// Bootanim-compatible string (`STAGE_TEXTS`) for on-screen banner.
pub fn bootanim_str() -> &'static str {
    match get_raw() {
        8 => "Enumerating PCI...",
        9 => "Starting input...",
        10 => "Probing storage...",
        11 => "Initializing USB...",
        12 => "Initializing audio...",
        13 => "Mounting filesystems...",
        14 => "Building namespace...",
        15 => "Starting scheduler...",
        16 => "Launching system...",
        0..=7 => "Initializing...",
        _ => "Running...",
    }
}
