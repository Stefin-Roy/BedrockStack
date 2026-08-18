//! BedrockOS audio subsystem.
//!
//! A small engine that probes the PCI bus for an Intel HD Audio controller
//! and exposes tone / PCM playback (and, when the codec has an ADC, capture)
//! to the rest of the kernel.  Currently x86_64-only — the riscv64 `virt`
//! machine has no PCI audio device, so the subsystem stays idle there
//! (`init()` is a no-op).
//!
//! The engine is a **feeding ring**: the HDA DMA ring for each direction is
//! started once at init and left running for the kernel's lifetime, and
//! playback/capture are driven synchronously by whoever calls them — there is
//! no kernel pump task and no request queue.  A call to `play_pcm`/
//! `record_pcm` stages the caller's samples into the next free DMA slot and
//! returns; when the ring is full (playback) or empty (capture) the caller
//! parks cooperatively until the DMA advances.  Back-to-back calls chain
//! gaplessly because the ring never stops, and a stalled producer plays
//! silence (the ISR zeroes each consumed output slot) instead of stale audio.
//! The HDA completion ISR is the only asynchronous actor: it bumps the
//! ring cursors and clears the just-consumed output slot.

pub mod codec;
pub mod hda;

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Once;

/// The fixed stream format the engine drives: 48 kHz, 16-bit signed, stereo.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// A playback/capture device the audio engine can drive.
///
/// Both directions are a **feeding ring**: the device keeps its DMA ring
/// running permanently, and each call synchronously moves data in or out of
/// the next available slot.  A call parks the caller (cooperatively in task
/// context, HLT in boot context) only when the ring is momentarily full
/// (playback) or momentarily empty (capture) — never for the duration of the
/// whole transfer.
pub trait AudioDevice: Send + Sync {
    fn name(&self) -> &str;

    /// Stage interleaved 16-bit signed stereo PCM at 48 kHz for playback.
    ///
    /// Copies `samples` into the next free DMA ring slot(s) and returns.  If
    /// the ring is full (the DMA has not yet consumed the oldest pending
    /// slots) the caller parks until a slot frees up, so a producer is paced
    /// to real time and back-to-back calls chain gaplessly with no seam.
    fn submit_playback(&self, samples: &[i16]) -> Result<(), &'static str>;

    /// True once capture is wired end-to-end (the codec has an ADC and its
    /// input path came up).  Playback-only devices return `false`.
    fn can_record(&self) -> bool {
        false
    }

    /// Read interleaved 16-bit signed stereo PCM at 48 kHz into `dest`.
    ///
    /// Copies the next captured DMA ring slot(s) into `dest`.  If no capture
    /// is available yet (the input DMA has not filled a slot) the caller
    /// parks until one arrives.
    fn read_capture(&self, dest: &mut [i16]) -> Result<(), &'static str> {
        let _ = dest;
        Err("capture not supported")
    }
}

static DEVICE: Once<&'static dyn AudioDevice> = Once::new();
static READY: AtomicBool = AtomicBool::new(false);

/// Probe for an audio controller and bring the first working one up.
///
/// Must run after `pci::init()`.  Non-fatal: no controller (or a failed
/// controller) leaves the subsystem idle and `is_ready()` false.
pub fn init() {
    // Re-entrancy guard: a second call would re-reset the live controller
    // (clobbering in-flight playback) and leak freshly-allocated DMA buffers,
    // so the subsystem only ever comes up once.
    if READY.load(Ordering::Acquire) {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    {
        use crate::drivers::serial::SerialPort;

        let mut found = false;
        // Prefer an Intel (vendor 0x8086) HD Audio controller: on laptops the
        // PCH's HDA function carries the analog codec (e.g. a Realtek) that
        // reaches the speakers, while a GPU's HDA function (NVIDIA/AMD) is an
        // HDMI/DP digital-only function with no path to the speakers.  Only
        // fall back to a non-Intel controller when no Intel one initialises.
        for prefer_intel in [true, false] {
            for dev in crate::pci::devices() {
                // Multimedia device → Audio device.
                if dev.class == 0x04 && dev.subclass == 0x03 {
                    if (dev.vendor_id == 0x8086) != prefer_intel {
                        continue;
                    }
                    found = true;
                    SerialPort::puts("[audio] found ");
                    SerialPort::put_u64(dev.bus as u64);
                    SerialPort::puts(":");
                    SerialPort::put_u64(dev.device as u64);
                    SerialPort::puts(":");
                    SerialPort::put_u64(dev.function as u64);
                    SerialPort::puts(" vid=0x");
                    SerialPort::put_hex(dev.vendor_id as u64);
                    SerialPort::puts(" did=0x");
                    SerialPort::put_hex(dev.device_id as u64);
                    SerialPort::puts("\n");
                    match hda::init(dev) {
                        Ok(d) => {
                            DEVICE.call_once(|| d);
                            READY.store(true, Ordering::Release);
                            SerialPort::puts("[audio] ready (");
                            SerialPort::puts(d.name());
                            SerialPort::puts(")\n");
                            return;
                        }
                        Err(e) => {
                            SerialPort::puts("[audio] hda init failed: ");
                            SerialPort::puts(e);
                            SerialPort::puts("\n");
                        }
                    }
                }
            }
        }
        if !found {
            SerialPort::puts("[audio] no HDA controller found\n");
        }
    }
}

/// True once a playback device is live and ready to accept samples.
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// The live device's name, when one is present (e.g. for introspection).
pub fn device_name() -> Option<&'static str> {
    DEVICE.get().map(|d| d.name())
}

/// Play interleaved 16-bit signed stereo PCM at 48 kHz.  Synchronous in the
/// feeding-ring sense: stages the samples into the running DMA ring and
/// returns once staged (parking only when the ring is momentarily full).
pub fn play_pcm(samples: &[i16]) -> Result<(), &'static str> {
    match DEVICE.get() {
        Some(d) => d.submit_playback(samples),
        None => Err("audio device not initialised"),
    }
}

/// True once the live device can capture (an ADC path is wired).
pub fn can_record() -> bool {
    match DEVICE.get() {
        Some(d) => d.can_record(),
        None => false,
    }
}

/// Record interleaved 16-bit signed stereo PCM at 48 kHz into `dest`.
/// Synchronous in the feeding-ring sense: reads the next captured DMA slot(s)
/// into `dest`, parking only when no capture has arrived yet.
pub fn record_pcm(dest: &mut [i16]) -> Result<(), &'static str> {
    match DEVICE.get() {
        Some(d) => d.read_capture(dest),
        None => Err("audio device not initialised"),
    }
}

/// Play a sine tone for `ms` milliseconds.  Synchronous in the feeding-ring
/// sense: synthesises the whole tone, then stages it into the running DMA
/// ring (parking only when the ring is momentarily full).  Rejects tones
/// longer than 10 seconds to avoid unbounded allocation.
pub fn play_tone(freq_hz: u32, ms: u64) -> Result<(), &'static str> {
    if ms > 10_000 {
        return Err("tone too long (max 10 s)");
    }
    let frames = (SAMPLE_RATE as u64 * ms / 1000) as usize;
    let mut samples = alloc::vec::Vec::with_capacity(frames * CHANNELS);
    samples.resize(frames * CHANNELS, 0i16);
    let step = 2.0 * core::f64::consts::PI * freq_hz as f64 / SAMPLE_RATE as f64;
    for f in 0..frames {
        let v = (sin_approx(step * f as f64) * 0.35 * 32767.0) as i16;
        samples[f * CHANNELS] = v;
        samples[f * CHANNELS + 1] = v;
    }
    play_pcm(&samples)
}

/// Bhaskara I rational approximation of `sin(x)`, valid on `[0, π]`.
///
/// The kernel is `no_std`, so the transcendental fns are unavailable; this
/// approximation keeps a pleasant sine wave (max error ≈ 1.8%) using only
/// `+ - * /`.
fn sin_approx(x: f64) -> f64 {
    let two_pi = 2.0 * core::f64::consts::PI;
    let x = x % two_pi;
    let (sign, xx) = if x > core::f64::consts::PI {
        (-1.0, x - core::f64::consts::PI)
    } else {
        (1.0, x)
    };
    let pi = core::f64::consts::PI;
    sign * 16.0 * xx * (pi - xx) / (5.0 * pi * pi - 4.0 * xx * (pi - xx))
}
