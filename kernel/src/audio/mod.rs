//! BedrockOS audio subsystem.
//!
//! A small engine that probes the PCI bus for an Intel HD Audio controller
//! and exposes tone / PCM playback to the rest of the kernel.  Currently
//! x86_64-only — the riscv64 `virt` machine has no PCI audio device, so the
//! subsystem stays idle there (`init()` is a no-op).
//!
//! Playback is queued and driven by a dedicated kernel pump task that feeds a
//! continuously-running HDA DMA ring, so `play_pcm`/`play_tone` return
//! immediately and back-to-back requests chain with no stop/start seam;
//! capture (`record_pcm`/`record_pcm_stream`) remains blocking and polled
//! until a future phase decouples it the same way.

pub mod codec;
pub mod hda;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
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

    /// Stream PCM through the DMA ring, feeding it from `next` until the
    /// closure returns `None`, then draining the final bytes and stopping.
    /// Unlike [`AudioDevice::play_pcm_stream`] the ring's transfer budget is
    /// effectively unbounded, so back-to-back data chains with **no
    /// stop/start seam**: the closure may keep pulling fresh chunks after one
    /// logical buffer ends.  Returns the number of bytes played.  The default
    /// collects the whole feed into one `play_pcm_stream` pass (gapless but
    /// with a single stop at the end); devices with a real cyclic ring
    /// override it to stream continuously.
    fn play_pcm_stream_continuous(
        &self,
        entry_bytes: usize,
        next: &mut dyn FnMut() -> Option<alloc::vec::Vec<i16>>,
    ) -> Result<u64, &'static str> {
        let mut chunks: alloc::vec::Vec<alloc::vec::Vec<i16>> = alloc::vec::Vec::new();
        let mut total: u64 = 0;
        while let Some(c) = next() {
            total += (c.len() * 2) as u64;
            chunks.push(c);
        }
        if total == 0 {
            return Err("stream ended early");
        }
        let mut it = chunks.into_iter();
        self.play_pcm_stream(total as u32, entry_bytes, &mut || it.next())
    }

    /// True once capture is wired end-to-end (the codec has an ADC and its
    /// input path came up).  Playback-only devices return `false`.
    fn can_record(&self) -> bool {
        false
    }

    /// Record interleaved 16-bit signed stereo PCM at 48 kHz into `dest`.
    /// Blocking; captures exactly `dest.len()` samples before returning.
    fn record_pcm(&self, dest: &mut [i16]) -> Result<(), &'static str> {
        let _ = dest;
        Err("capture not supported")
    }

    /// Record PCM through a continuously-running DMA ring, so capture is
    /// gapless and exactly real-time.  Blocking until the whole stream has
    /// been captured.
    ///
    /// `total_bytes` is the exact payload size and `entry_bytes` the size of
    /// every chunk except the last (which holds the remainder); `sink`
    /// receives the captured chunks in order, as owned copies (the ring is
    /// live DMA memory being overwritten by the controller).  Returns the
    /// number of bytes recorded.
    fn record_pcm_stream(
        &self,
        total_bytes: u32,
        entry_bytes: usize,
        sink: &mut dyn FnMut(alloc::vec::Vec<i16>),
    ) -> Result<u64, &'static str> {
        let _ = (total_bytes, entry_bytes, sink);
        Err("capture not supported")
    }
}

/// The fixed stream format the engine drives: 48 kHz, 16-bit signed, stereo.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

static DEVICE: Once<&'static dyn AudioDevice> = Once::new();
static READY: AtomicBool = AtomicBool::new(false);

// ── Playback pump ─────────────────────────────────────────────────
//
// Playback is queued and driven by a dedicated kernel task (the pump) that
// feeds the HDA streaming ring continuously, so a `:play_pcm`/`:play_tone`
// call enqueues and returns immediately instead of HLTing the BSP for the
// whole duration (old AUD-028 behaviour).  The pump chains queued requests
// into one continuous DMA session with no stop/start seam between them, and
// parks in short slices while waiting for ring completions, letting the rest
// of the system flow.

/// Maximum queued playback requests.  Bounded so a flood of `:play_pcm`
/// cannot exhaust the heap; enqueueing callers park cooperatively until the
/// pump drains a slot.
const PUMP_QUEUE_CAP: usize = 8;
/// Ring slot size fed by the pump — must equal `BUF_CAP / RING_ENTRIES`
/// (256 KiB / 8 = 32 KiB) in `hda.rs`.
const STREAM_SLOT: usize = 32 * 1024;
/// Pump wake cadence while waiting for a ring completion (~1 ms slices keep
/// refill latency far below the ~170 ms per-slot DMA headroom).
const PUMP_POLL_SLICE: u64 = 1_000_000;
/// Grace window the pump waits (in the feed closure) for a request enqueued
/// just as the queue empties, so a near-back-to-back `:play_pcm` still chains
/// into the running session instead of restarting with a seam.
const PUMP_GRACE_NS: u64 = 10_000_000;

static PUMP_QUEUE: spin::Mutex<VecDeque<Vec<i16>>> = spin::Mutex::new(VecDeque::new());
/// Whether the pump task is running (spawned after `task::init`, so
/// boot-context callers fall back to the legacy blocking path).
static PUMP_ALIVE: AtomicBool = AtomicBool::new(false);

/// The pump's session counter — how many continuous sessions have completed,
/// so a blocking caller (if any) could wait on progress.  Currently used for
/// diagnostics only.
static PUMP_SESSIONS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Queue `samples` (interleaved 16-bit signed stereo, 48 kHz) for playback.
///
/// Returns immediately: the pump plays it through the streaming ring and
/// chains back-to-back requests seamlessly.  If the pump is not running
/// (boot context, no device, or spawn failed) this falls back to the legacy
/// blocking one-shot `play_pcm`.  When the bounded queue is full, the caller
/// parks cooperatively (yielding to the scheduler) until the pump frees a
/// slot — nothing ever HLTs the BSP here.
pub fn enqueue_playback(samples: Vec<i16>) -> Result<(), &'static str> {
    if samples.is_empty() {
        return Err("empty PCM");
    }
    crate::drivers::serial::SerialPort::puts("[audio] enqueue: called\n");
    if !PUMP_ALIVE.load(Ordering::Acquire) {
        return match DEVICE.get() {
            Some(d) => d.play_pcm(&samples),
            None => Err("audio device not initialised"),
        };
    }
    loop {
        {
            let mut q = PUMP_QUEUE.lock();
            if q.len() < PUMP_QUEUE_CAP {
                q.push_back(samples);
                return Ok(());
            }
        }
        crate::drivers::serial::SerialPort::puts("[audio] enqueue: queue full, parking\n");
        crate::task::sleep_current(PUMP_POLL_SLICE);
    }
}

/// Spawn the playback pump kernel task.  Call once from `Kernel::run()`
/// after `task::init()` (and only when an audio device came up).  No-op if
/// there is no device or the pump already runs.
#[cfg(target_arch = "x86_64")]
pub fn spawn_pump(alloc: &mut crate::mm::phys_alloc::BitmapAllocator) {
    if !is_ready() {
        return;
    }
    if PUMP_ALIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    let root = crate::task::kernel_root();
    let (top, slot) = match crate::task::alloc_kernel_stack(alloc) {
        Some(v) => v,
        None => {
            PUMP_ALIVE.store(false, Ordering::Release);
            crate::drivers::serial::SerialPort::puts("[audio] pump: no kernel stack\n");
            return;
        }
    };
    // Entry RSP must be 8 mod 16 (SysV callee entry) — top minus 8.
    let mut task = crate::task::Task::new(
        top,
        root,
        0,
        crate::task::TaskContext::new(top - 8, audio_pump_entry as *const () as usize as u64),
    );
    task.kstack_slot = slot;
    crate::task::spawn(task);
    crate::drivers::serial::SerialPort::puts("[audio] pump task spawned\n");
}

/// The pump task body.  Explicit ABI-stable entry (a closure coerced to
/// `fn()` would enter through an `FnOnce` shim, but `switch_to` starts from a
/// fabricated context).  Lives for the kernel's lifetime: pops queued
/// requests and feeds them to the continuous ring, chaining back-to-back
/// requests with no seam, then parks until the next enqueue.
#[cfg(target_arch = "x86_64")]
extern "C" fn audio_pump_entry() -> ! {
    use crate::drivers::serial::SerialPort as SP;
    static FIRST_RUN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !FIRST_RUN.swap(true, core::sync::atomic::Ordering::AcqRel) {
        SP::puts("[audio] pump: first run\n");
    }
    loop {
        let first = {
            let mut q = PUMP_QUEUE.lock();
            q.pop_front()
        };
        let Some(first) = first else {
            crate::task::sleep_current(PUMP_POLL_SLICE);
            continue;
        };
        SP::puts("[audio] pump: popped ");
        SP::put_u64((first.len() * 2) as u64);
        SP::puts(" B\n");

        // Feed the session.  Each slot must be filled with ~170 ms of real
        // samples before being returned, so playback runs at real time: drain
        // the current request, then chain more requests from the queue until
        // the slot is full.  When the queue sits empty past the grace window
        // the slot is returned as-is (a short tail is zero-padded by the
        // engine), and once nothing is left at all the closure returns `None`
        // so the engine drains and stops with no stop/start seam.
        let mut current = first;
        let mut pos = 0usize;
        let slot_samples = STREAM_SLOT / 2;
        let r = play_pcm_stream_continuous(STREAM_SLOT, &mut || {
            let mut slot: Vec<i16> = Vec::with_capacity(slot_samples);
            while slot.len() < slot_samples {
                if pos >= current.len() {
                    let grace_deadline = crate::services::universal_timer::now_ns() + PUMP_GRACE_NS;
                    let mut got = false;
                    loop {
                        let next = {
                            let mut q = PUMP_QUEUE.lock();
                            q.pop_front()
                        };
                        match next {
                            Some(next) => {
                                current = next;
                                pos = 0;
                                got = true;
                                break;
                            }
                            None => {
                                if crate::services::universal_timer::now_ns() >= grace_deadline {
                                    break;
                                }
                                crate::task::sleep_current(PUMP_POLL_SLICE);
                            }
                        }
                    }
                    if !got && pos >= current.len() {
                        // Queue empty past grace: the session is over.
                        break;
                    }
                    continue;
                }
                let take = (slot_samples - slot.len()).min(current.len() - pos);
                slot.extend_from_slice(&current[pos..pos + take]);
                pos += take;
            }
            if slot.is_empty() { None } else { Some(slot) }
        });

        let _ = PUMP_SESSIONS.fetch_add(1, Ordering::Relaxed);
        match r {
            Ok(b) => {
                SP::puts("[audio] pump played ");
                SP::put_u64(b);
                SP::puts(" B\n");
            }
            Err(e) => {
                SP::puts("[audio] pump: ");
                SP::puts(e);
                SP::puts("\n");
            }
        }
    }
}

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

/// The live device's name, when one is present (e.g. for introspection).
pub fn device_name() -> Option<&'static str> {
    DEVICE.get().map(|d| d.name())
}

/// Play interleaved 16-bit signed stereo PCM at 48 kHz.  Asynchronous: the
/// samples are queued for the pump task, which plays them through the
/// streaming ring (gapless) and returns the call immediately.  Without a
/// running pump this falls back to the legacy blocking one-shot path.
pub fn play_pcm(samples: &[i16]) -> Result<(), &'static str> {
    enqueue_playback(samples.to_vec())
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

/// Stream PCM through the device's DMA ring, chaining back-to-back data with
/// no stop/start seam until `next` returns `None`.  See
/// [`AudioDevice::play_pcm_stream_continuous`].
pub fn play_pcm_stream_continuous(
    entry_bytes: usize,
    next: &mut dyn FnMut() -> Option<alloc::vec::Vec<i16>>,
) -> Result<u64, &'static str> {
    match DEVICE.get() {
        Some(d) => d.play_pcm_stream_continuous(entry_bytes, next),
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
/// Blocking; mirrors `play_pcm`.
pub fn record_pcm(dest: &mut [i16]) -> Result<(), &'static str> {
    match DEVICE.get() {
        Some(d) => d.record_pcm(dest),
        None => Err("audio device not initialised"),
    }
}

/// Record PCM through the device's DMA ring (gapless, real-time).  See
/// [`AudioDevice::record_pcm_stream`].
pub fn record_pcm_stream(
    total_bytes: u32,
    entry_bytes: usize,
    sink: &mut dyn FnMut(alloc::vec::Vec<i16>),
) -> Result<u64, &'static str> {
    match DEVICE.get() {
        Some(d) => d.record_pcm_stream(total_bytes, entry_bytes, sink),
        None => Err("audio device not initialised"),
    }
}

/// Play a sine tone for `ms` milliseconds.  Asynchronous: enqueued for the
/// pump, which plays it gaplessly through the streaming ring.
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
    enqueue_playback(samples)
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
