//! BedrockOS audio subsystem.
//!
//! A small engine that probes the PCI bus for an Intel HD Audio controller
//! and exposes tone / PCM playback to the rest of the kernel.  Currently
//! x86_64-only — the riscv64 `virt` machine has no PCI audio device, so the
//! subsystem stays idle there (`init()` is a no-op).
//!
//! Playback is blocking and polled: `play_tone`/`play_pcm` synthesise (or
//! copy) interleaved 16-bit signed stereo samples at 48 kHz into a DMA
//! staging buffer, then drive the HDA output stream to completion.

pub mod codec;
pub mod hda;

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Once;

/// A playback device the audio engine can drive.
pub trait AudioDevice: Send + Sync {
    fn name(&self) -> &str;

    /// Play interleaved 16-bit signed stereo PCM at 48 kHz.  Blocking.
    fn play_pcm(&self, samples: &[i16]) -> Result<(), &'static str>;

    /// Stream interleaved 16-bit signed stereo PCM at 48 kHz through a
    /// continuously-running DMA ring, so playback is gapless and exactly
    /// real-time.  Blocking until the whole stream has been consumed.
    ///
    /// `total_bytes` is the exact payload size and `entry_bytes` the size of
    /// every chunk except the last (which holds the remainder); `next`
    /// supplies the chunks in order and must return `None` once `total_bytes`
    /// have been delivered.  Returns the number of bytes played.
    fn play_pcm_stream(
        &self,
        total_bytes: u32,
        entry_bytes: usize,
        next: &mut dyn FnMut() -> Option<alloc::vec::Vec<i16>>,
    ) -> Result<u64, &'static str>;
}

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;

static DEVICE: Once<&'static dyn AudioDevice> = Once::new();
static READY: AtomicBool = AtomicBool::new(false);

/// Probe for an audio controller and bring the first working one up.
///
/// Must run after `pci::init()`.  Non-fatal: no controller (or a failed
/// controller) leaves the subsystem idle and `is_ready()` false.
pub fn init() {
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

/// Play interleaved 16-bit signed stereo PCM at 48 kHz.  Blocking.
pub fn play_pcm(samples: &[i16]) -> Result<(), &'static str> {
    match DEVICE.get() {
        Some(d) => d.play_pcm(samples),
        None => Err("audio device not initialised"),
    }
}

/// Stream PCM through the device's DMA ring (gapless, real-time).  See
/// [`AudioDevice::play_pcm_stream`].
pub fn play_pcm_stream(
    total_bytes: u32,
    entry_bytes: usize,
    next: &mut dyn FnMut() -> Option<alloc::vec::Vec<i16>>,
) -> Result<u64, &'static str> {
    match DEVICE.get() {
        Some(d) => d.play_pcm_stream(total_bytes, entry_bytes, next),
        None => Err("audio device not initialised"),
    }
}

/// Play a sine tone for `ms` milliseconds.  Blocking.
pub fn play_tone(freq_hz: u32, ms: u64) -> Result<(), &'static str> {
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
