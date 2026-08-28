//! Boot animation: hex logo + indeterminate pulse + stage text.
//!
//! Tier 2 design (30 fps, x86_64 only):
//!   - Dark background `#0F111A`, hex logo with accent border, `BedrockOS`.
//!   - Indeterminate pulse track (mono-directional sweep) with stage text
//!     below it (`Initializing...` -> `Launching system...`).
//!   - Stage text driven by `set_stage()` atomics — the ISR never blocks.
//!   - `stop()` paints black efficiently with a single `clear()` +
//!     `flush_full()` (one memset + one VRAM copy, no per-pixel loops).
//!
//! Early path: `early_show()` paints the static BGRT/hex + `BedrockOS` without
//! the track/pulse immediately after ACPI BGRT is ready (no timer, no IRQ).
//! `enable_bar()` adds the track/stage and arms the 30 fps sweep after
//! interrupts are live.
//!
//! ISR safety: `sweep_tick` runs in LAPIC-timer ISR context with the timer
//! base's queue lock held (`universal_timer::UniversalTimerImpl::tick`). It
//! must stay lock-free, panic-free and allocation-free: only atomics +
//! shadow pokes + a single `flush()`.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use framebuffer::{Color, Display, Framebuffer};
use spin::Mutex;

use crate::services::universal_timer;

// ── timing / geometry ──────────────────────────────────────────────────

/// 30 fps — halves the timer IRQ rate vs the original 60 fps (16 ms).
const SWEEP_PERIOD_NS: u64 = 33_333_333;

/// Height of the pulse track.
const TRACK_H: usize = 6;

/// Hex logo outer radius (pointy-top, centre->vertex).
const HEX_RADIUS: usize = 42;
const HEX_INNER_RADIUS: usize = 34;

/// Palette.
const BG: Color = Color::new(15, 17, 26, 255);
const TRACK_BG: Color = Color::new(38, 42, 56, 255);
const ACCENT: Color = Color::new(90, 242, 255, 255);
const FG_TEXT: Color = Color::WHITE;

/// Stage strings — indexed by `STAGE` atomic.
const STAGE_TEXTS: &[&str] = &[
    "Initializing...",
    "Enumerating PCI...",
    "Starting input...",
    "Probing storage...",
    "Initializing USB...",
    "Initializing audio...",
    "Mounting filesystems...",
    "Building namespace...",
    "Starting scheduler...",
    "Launching system...",
];

// ── shared atomics ─────────────────────────────────────────────────────

static FRAME: AtomicU64 = AtomicU64::new(0);
static STAGE: AtomicU8 = AtomicU8::new(0);
static TIMER_ID: Mutex<Option<universal_timer::TimerId>> = Mutex::new(None);
static FB_PTR: AtomicU64 = AtomicU64::new(0);

// ── public API ─────────────────────────────────────────────────────────

/// Boot stage for `set_stage`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Init = 0,
    Pci = 1,
    Input = 2,
    Storage = 3,
    Usb = 4,
    Audio = 5,
    Vfs = 6,
    Namespace = 7,
    Scheduler = 8,
    Launch = 9,
}

/// Draw the static boot screen and arm the 30 fps sweep.
///
/// Kept for compatibility: draws the full screen (with track) and arms.
pub fn start(fb: &mut Framebuffer) {
    if crate::bootargs::is_nobootanim() {
        return;
    }
    if fb.width() == 0 || fb.height() == 0 || fb.bpp() == 0 {
        return;
    }
    // If early_show was already called, just arm the bar.
    let already_shown = FB_PTR.load(Ordering::Relaxed) != 0;
    if already_shown {
        enable_bar();
        return;
    }
    FRAME.store(0, Ordering::Relaxed);
    STAGE.store(0, Ordering::Relaxed);
    draw_static(fb, true);
    let ctx = fb as *mut Framebuffer as *mut u8;
    FB_PTR.store(ctx as u64, Ordering::Relaxed);
    let id = universal_timer::universal_timer().set_periodic(SWEEP_PERIOD_NS, sweep_tick, ctx);
    *TIMER_ID.lock() = Some(id);
}

/// Draw the early static screen (BGRT/hex + `BedrockOS`) without the
/// indeterminate track. Called immediately after BGRT parse + shadow
/// allocation, before interrupts are enabled. No timer is armed.
pub fn early_show(fb: &mut Framebuffer) {
    if crate::bootargs::is_nobootanim() {
        return;
    }
    if fb.width() == 0 || fb.height() == 0 || fb.bpp() == 0 {
        return;
    }
    // If already shown (e.g., via start), keep the later full draw.
    if FB_PTR.load(Ordering::Relaxed) != 0 {
        return;
    }
    FRAME.store(0, Ordering::Relaxed);
    STAGE.store(0, Ordering::Relaxed);
    draw_static(fb, false);
    let ctx = fb as *mut Framebuffer as *mut u8;
    FB_PTR.store(ctx as u64, Ordering::Relaxed);
}

/// Add the indeterminate track/stage and arm the 30 fps sweep.
/// No-op if already armed or if `early_show` was never called.
pub fn enable_bar() {
    if crate::bootargs::is_nobootanim() {
        return;
    }
    if TIMER_ID.lock().is_some() {
        return;
    }
    let ptr_val = FB_PTR.load(Ordering::Relaxed);
    if ptr_val == 0 {
        return;
    }
    let fb = unsafe { &mut *(ptr_val as *mut Framebuffer) };
    if fb.width() == 0 || fb.height() == 0 || fb.bpp() == 0 || fb.shadow_ptr().is_null() {
        return;
    }
    // Paint the track + initial stage text on top of the early static image,
    // then flush the delta before arming so the first pulse frame is coherent.
    let w = fb.width();
    let h = fb.height();
    let hex_y = hex_center_y(h);
    let bedrock_y = bedrock_y(hex_y);
    draw_track(fb, w, h, bedrock_y);
    let s = STAGE.load(Ordering::Relaxed);
    draw_stage_text(fb, w, h, bedrock_y, s);
    fb.flush();

    // Arm the sweep. Use the stored FB_PTR as context (same as start).
    let ctx = ptr_val as *mut u8;
    let id = universal_timer::universal_timer().set_periodic(SWEEP_PERIOD_NS, sweep_tick, ctx);
    *TIMER_ID.lock() = Some(id);
}

/// Cancel the sweep and paint the screen black efficiently.
///
/// Black is `clear()` (`write_bytes 0` over the shadow) + a single
/// `flush_full()` (`copy_nonoverlapping` whole VRAM). One memset plus one
/// bulk copy — no per-pixel loops, no dirty-rect walk.
pub fn stop() {
    if crate::bootargs::is_nobootanim() {
        // No animation was ever started, but still clear the parked ptr.
        FB_PTR.store(0, Ordering::Relaxed);
        FRAME.store(0, Ordering::Relaxed);
        STAGE.store(0, Ordering::Relaxed);
        return;
    }
    if let Some(id) = TIMER_ID.lock().take() {
        universal_timer::universal_timer().cancel(id);
    }
    let ptr_val = FB_PTR.swap(0, Ordering::Relaxed);
    if ptr_val != 0 {
        unsafe {
            let ptr = ptr_val as *mut Framebuffer;
            if !ptr.is_null() {
                let fb = &mut *ptr;
                if !fb.shadow_ptr().is_null() && fb.width() > 0 && fb.height() > 0 {
                    fb.clear();
                    fb.flush_full();
                }
            }
        }
    }
    FRAME.store(0, Ordering::Relaxed);
    STAGE.store(0, Ordering::Relaxed);
}

/// Update the stage text shown below the track. Lock-free, ISR-visible.
pub fn set_stage(stage: Stage) {
    // Always publish to kernel stage tracker (for dumps/screens) even when
    // bootanim is disabled — map bootanim 0..9 -> kernel 7..16.
    crate::stage::set_raw(stage as u8 + 7);
    if crate::bootargs::is_nobootanim() {
        return;
    }
    STAGE.store(stage as u8, Ordering::Relaxed);
}

/// Raw index variant for call sites that don't want the enum.
pub fn set_stage_raw(idx: u8) {
    crate::stage::set_raw(idx.saturating_add(7).min(16));
    if crate::bootargs::is_nobootanim() {
        return;
    }
    let max = (STAGE_TEXTS.len() as u8).saturating_sub(1);
    STAGE.store(idx.min(max), Ordering::Relaxed);
}

// ── static drawing ─────────────────────────────────────────────────────

fn draw_static(fb: &mut Framebuffer, with_track: bool) {
    let (w, h) = (fb.width(), fb.height());

    fb.fill_solid(BG);

    let cx = w / 2;
    let hex_y = hex_center_y(h);

    // Try UEFI BGRT logo (TianoCore etc) — falls back to hex if absent/invalid.
    let bgrt_ok = try_bgrt_logo(fb, cx, hex_y).is_some();
    if !bgrt_ok {
        draw_hex_with_border(fb, cx, hex_y, HEX_RADIUS, HEX_INNER_RADIUS, ACCENT, BG);
    }

    let bedrock_y = bedrock_y(hex_y);
    draw_centered_colored(fb, w, bedrock_y, "BedrockOS", FG_TEXT, BG);

    if with_track {
        draw_track(fb, w, h, bedrock_y);
        let s = STAGE.load(Ordering::Relaxed);
        draw_stage_text(fb, w, h, bedrock_y, s);
    }

    fb.flush_full();
}

fn try_bgrt_logo(fb: &mut Framebuffer, cx: usize, cy: usize) -> Option<(usize, usize)> {
    #[cfg(target_arch = "x86_64")]
    {
        return crate::acpi::bgrt::blit_bgrt_logo(fb, cx, cy);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (fb, cx, cy);
        None
    }
}

fn draw_centered_colored(fb: &mut Framebuffer, w: usize, y: usize, text: &str, fg: Color, bg: Color) {
    let tw = text.len() * 8;
    let x = w.saturating_sub(tw) / 2;
    for (i, b) in text.bytes().enumerate() {
        let _ = fb.draw_char_colored(x + i * 8, y, b, fg, bg);
    }
}

// ── track ───────────────────────────────────────────────────────────────

fn draw_track(fb: &mut Framebuffer, w: usize, h: usize, bedrock_y: usize) {
    let track_w = track_width(w);
    let track_x = (w - track_w) / 2;
    let track_y = track_y_for(bedrock_y, h);
    fb.fill_rect(track_x, track_y, track_w, 1, Color::GRAY);
    fb.fill_rect(track_x, track_y + TRACK_H - 1, track_w, 1, Color::GRAY);
    fb.fill_rect(track_x, track_y + 1, track_w, TRACK_H - 2, TRACK_BG);
}

// ── hex logo ────────────────────────────────────────────────────────────

fn draw_hex_with_border(
    fb: &mut Framebuffer,
    cx: usize,
    cy: usize,
    outer_r: usize,
    inner_r: usize,
    outer: Color,
    inner: Color,
) {
    draw_filled_hex(fb, cx, cy, outer_r, outer);
    draw_filled_hex(fb, cx, cy, inner_r, inner);
}

/// Pointy-top filled hexagon centred at (cx,cy) with circum-radius `r`.
fn draw_filled_hex(fb: &mut Framebuffer, cx: usize, cy: usize, r: usize, color: Color) {
    if r == 0 || fb.shadow_ptr().is_null() {
        return;
    }
    let w = fb.width();
    let h = fb.height();
    let w_half = r * 866 / 1000; // ≈ 0.866R
    let r_half = r / 2;
    for dy in -(r as isize)..=r as isize {
        let y = cy as isize + dy;
        if y < 0 || y >= h as isize {
            continue;
        }
        let abs = dy.abs() as usize;
        let half = if abs <= r_half {
            w_half
        } else {
            let remaining = r - abs;
            if r_half == 0 {
                0
            } else {
                w_half * remaining / r_half
            }
        };
        if half == 0 {
            if cx < w {
                fb.fill_rect(cx, y as usize, 1, 1, color);
            }
        } else {
            let x0 = cx.saturating_sub(half);
            let width = (half * 2).min(w.saturating_sub(x0));
            if width > 0 && x0 < w {
                fb.fill_rect(x0, y as usize, width, 1, color);
            }
        }
    }
}

// ── stage text ──────────────────────────────────────────────────────────

fn draw_stage_text(fb: &mut Framebuffer, w: usize, h: usize, bedrock_y: usize, stage: u8) {
    let track_y = track_y_for(bedrock_y, h);
    let y = stage_y(track_y);
    if y + 16 > h {
        return;
    }
    let idx = (stage as usize).min(STAGE_TEXTS.len() - 1);
    let text = STAGE_TEXTS[idx];
    let erase_w = 420.min(w);
    let erase_x = w.saturating_sub(erase_w) / 2;
    fb.fill_rect(erase_x, y, erase_w, 16, BG);
    draw_centered_colored(fb, w, y, text, FG_TEXT, BG);
}

// ── ISR tick ────────────────────────────────────────────────────────────

fn sweep_tick(context: *mut u8) {
    let fb = unsafe { &mut *(context as *mut Framebuffer) };
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);
    draw_frame(fb, frame);
}

fn draw_frame(fb: &mut Framebuffer, frame: u64) {
    let (w, h) = (fb.width(), fb.height());
    if w == 0 || h == 0 {
        return;
    }
    let bedrock_y = bedrock_y(hex_center_y(h));

    // Track pulse — indeterminate sweep.
    let track_w = track_width(w);
    let pulse_w = pulse_width(track_w);
    let track_x = (w - track_w) / 2;
    let track_y = track_y_for(bedrock_y, h);

    fb.fill_rect(track_x, track_y + 1, track_w, TRACK_H - 2, TRACK_BG);
    let full = track_w + pulse_w;
    let step = (pulse_w / 2).max(1);
    let pos = ((frame as usize) * step) % full;
    let p0 = track_x as isize + pos as isize - pulse_w as isize;
    let p1 = p0 + pulse_w as isize;
    let x0 = p0.max(track_x as isize);
    let x1 = p1.min((track_x + track_w) as isize);
    if x1 > x0 {
        fb.fill_rect(
            x0 as usize,
            track_y + 1,
            (x1 - x0) as usize,
            TRACK_H - 2,
            ACCENT,
        );
    }

    let stage = STAGE.load(Ordering::Relaxed);
    draw_stage_text(fb, w, h, bedrock_y, stage);

    fb.flush();
}

// ── layout helpers ──────────────────────────────────────────────────────

fn hex_center_y(h: usize) -> usize {
    if h < 480 {
        h / 3
    } else {
        h / 2 - 90
    }
}

fn bedrock_y(hex_y: usize) -> usize {
    hex_y + HEX_RADIUS + 16
}

fn track_y_for(bedrock_y: usize, h: usize) -> usize {
    let y = bedrock_y + 38;
    let need = y + TRACK_H + 12 + 16 + 8;
    if need > h {
        h.saturating_sub(TRACK_H + 12 + 16 + 8)
    } else {
        y
    }
}

fn stage_y(track_y: usize) -> usize {
    track_y + TRACK_H + 12
}

fn track_width(w: usize) -> usize {
    ((w * 2) / 3).clamp(360, 720).min(w.saturating_sub(32))
}

fn pulse_width(track_w: usize) -> usize {
    (track_w / 6).max(48)
}
