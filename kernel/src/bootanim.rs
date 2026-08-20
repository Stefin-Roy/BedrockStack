//! Boot animation: an indeterminate "pulse" spinner.
//!
//! Driven by a periodic universal-timer callback, so it animates for the
//! entire `Kernel::run()` boot tail (PCI, storage, USB, audio, VFS) and
//! stops the moment user-space INIT takes over the screen.
//!
//! The callback runs in LAPIC-timer ISR context with the timer base's queue
//! lock held, so it must stay lock-free and panic-free: it only pokes the
//! shadow framebuffer and flushes the dirty rectangle.

use core::sync::atomic::{AtomicU64, Ordering};
use framebuffer::{Color, Display, Framebuffer};
use spin::Mutex;

use crate::services::universal_timer;

/// Sweep cadence: 16 ms ≈ 60 fps.
const SWEEP_PERIOD_NS: u64 = 16_000_000;

/// Height of the pulse track in pixels.
const TRACK_H: usize = 6;

static FRAME: AtomicU64 = AtomicU64::new(0);
static TIMER_ID: Mutex<Option<universal_timer::TimerId>> = Mutex::new(None);

/// Draw the static boot screen and arm the periodic sweep on `fb`.
pub fn start(fb: &mut Framebuffer) {
    draw_static(fb);
    let ctx = fb as *mut Framebuffer as *mut u8;
    let id = universal_timer::universal_timer().set_periodic(SWEEP_PERIOD_NS, sweep_tick, ctx);
    *TIMER_ID.lock() = Some(id);
}

/// Cancel the sweep. Called right before INIT is launched; INIT's desktop
/// paint covers the spinner.
pub fn stop() {
    if let Some(id) = TIMER_ID.lock().take() {
        universal_timer::universal_timer().cancel(id);
    }
}

fn draw_static(fb: &mut Framebuffer) {
    fb.clear();
    let (w, h) = (fb.width(), fb.height());
    draw_centered(fb, w, h / 3, "BEDROCK OS");
    draw_centered(fb, w, h / 3 + 24, "booting...");
    draw_track(fb, w, h);
    fb.flush_full();
}

fn draw_centered(fb: &mut Framebuffer, w: usize, y: usize, text: &str) {
    let tw = text.len() * 8;
    let x = w.saturating_sub(tw) / 2;
    for (i, b) in text.bytes().enumerate() {
        fb.draw_char(x + i * 8, y, b);
    }
}

fn draw_track(fb: &mut Framebuffer, w: usize, h: usize) {
    let track_w = track_width(w);
    let track_x = (w - track_w) / 2;
    let track_y = track_y(h);
    fb.fill_rect(track_x, track_y, track_w, 1, Color::GRAY);
    fb.fill_rect(track_x, track_y + TRACK_H - 1, track_w, 1, Color::GRAY);
    fb.fill_rect(track_x, track_y + 1, track_w, TRACK_H - 2, track_fill());
}

fn sweep_tick(context: *mut u8) {
    let fb = unsafe { &mut *(context as *mut Framebuffer) };
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);
    draw_frame(fb, frame);
}

fn draw_frame(fb: &mut Framebuffer, frame: u64) {
    let (w, h) = (fb.width(), fb.height());
    let track_w = track_width(w);
    let pulse_w = pulse_width(track_w);
    let track_x = (w - track_w) / 2;
    let track_y = track_y(h);

    // Redraw the interior so the previous pulse is erased, then clip the
    // swept pulse to the track bounds before committing.
    fb.fill_rect(track_x, track_y + 1, track_w, TRACK_H - 2, track_fill());
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
            Color::CYAN,
        );
    }
    fb.flush();
}

fn track_width(w: usize) -> usize {
    w / 2
}

fn pulse_width(track_w: usize) -> usize {
    (track_w / 5).max(40)
}

fn track_y(h: usize) -> usize {
    h * 3 / 4
}

fn track_fill() -> Color {
    Color::new(38, 42, 56, 255)
}