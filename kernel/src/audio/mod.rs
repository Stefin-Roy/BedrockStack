//! BedrockOS audio subsystem.
//!
//! A small engine that probes the PCI bus for an Intel HD Audio controller
//! and exposes tone / PCM playback (and, when the codec has an ADC, capture)
//! to the rest of the kernel.  Currently x86_64-only — the riscv64 `virt`
//! machine has no PCI audio device, so the subsystem stays idle there
//! (`init()` is a no-op).
//!
//! The engine is a **feeding ring** + **fire-forget pump**: the HDA DMA ring
//! for each direction is started once at init and left running for the
//! kernel's lifetime (isr zeroes consumed slots → silence, no stale-loop
//! repetition after provider finished). Playback `play_pcm`/`play_tone` are
//! **non-blocking** when the pump task is alive: samples are queued into a
//! bounded `PUMP_QUEUE` (cap 4) and the caller returns immediately — the pump
//! chains them gaplessly through the running ring. When the ring is full the
//! pump parks cooperatively (never HLTs BSP) until DMA advances; when the pump
//! is not yet alive (boot context, no device) the call falls back to the
//! legacy synchronous feeding-ring submit so early boot still works.
//! Capture `record_pcm` remains synchronous blocking (future work may decouple
//! it).

pub mod codec;
pub mod hda;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
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

// ── Playback pump ───────────────────────────────────────────────────
//
// The old pump (`212a754`) used a `CBL=u32::MAX` cyclic BDL that kept the
// DMA cycling forever with stale PCM after the provider finished — the ring
// repetition bug. The current feeding ring is always-running with ISR zeroing
// (hda.rs `out_completed_reconcile` zeroes each consumed `delta`), so drained
// slots play silence. The pump therefore simply dequeues Vec<i16> chunks and
// forwards them to the feeding ring's `submit_playback`; DMA never replays
// stale data because the isr clears it, and the pump never touches BDL
// directly. This keeps fire-forget (callers enqueue and return) without the
// repetition hazard.
//
// Bounded queue: 4 entries × up to ~8188 B (DOOM chunk) ≈ 32 KiB, plus
// occasional tone (10s → 1.9 MiB) still fits but a large tone will occupy
// one slot. Enqueuers park cooperatively (task::sleep_current 0.5ms slices)
// when full, never HLT the BSP. Total pipeline with the 16×1024 ring
// (≈85 ms staged cap ≈80 ms) is ≈ 255 ms worst before enqueuers park.

/// Keep queue shallow to bound latency: 16×1024 ring ≈80 ms staged cap plus
/// 4× ≈42.6 ms DOOM chunks ≈ 250 ms worst. Fire-forget still holds —
/// enqueuers park only when truly full.
const PUMP_QUEUE_CAP: usize = 4;
static PUMP_QUEUE: crate::filesystems::vfs::irq::IrqMutex<VecDeque<Vec<i16>>> = crate::filesystems::vfs::irq::IrqMutex::new(VecDeque::new());
static PUMP_ALIVE: AtomicBool = AtomicBool::new(false);

/// Persistent sine phase (raw f64 bits) across `play_tone` calls, so
/// consecutive tones chain without a phase-reset click.
static TONE_PHASE_BITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Queue `samples` for the pump. Returns immediately; the pump plays it
/// through the feeding ring. If the pump is not running (boot context, no
/// device, spawn failed) falls back to synchronous `submit_playback` so early
/// boot still audible. When the bounded queue is full, the caller parks
/// cooperatively (yielding) until the pump frees a slot — never HLTs the BSP
/// when a task context exists (AUD-028).
fn enqueue_playback(samples: Vec<i16>) -> Result<(), &'static str> {
    if samples.is_empty() {
        return Ok(());
    }
    if !PUMP_ALIVE.load(Ordering::Acquire) {
        // Pump not alive: boot context or no device — synchronous fallback.
        return match DEVICE.get() {
            Some(d) => d.submit_playback(&samples),
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
        // Queue full — park cooperatively. If no current task (should not
        // happen when pump is alive, but be safe) fall back to HLT wait.
        #[cfg(target_arch = "x86_64")]
        {
            if crate::smp::current_per_cpu().current_task.load(core::sync::atomic::Ordering::Relaxed).is_null() {
                crate::services::universal_timer::sleep_ms(1);
            } else {
                crate::task::sleep_current(500_000);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            crate::services::universal_timer::sleep_ms(1);
        }
    }
}

/// Spawn the playback pump kernel task. Call once from `Kernel::run()` after
/// `task::init()` and only when an audio device is live. No-op if already
/// running or no device. The pump lives for the kernel lifetime: pops queued
/// requests and forwards them to the feeding ring's `submit_playback`, which
/// handles the real-time pacing and ISR zeroing — so there is no ring
/// repetition after the provider finishes (drained slots are silence).
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

#[cfg(target_arch = "x86_64")]
extern "C" fn audio_pump_entry() -> ! {
    use crate::drivers::serial::SerialPort as SP;
    SP::puts("[audio] pump: entry\n");
    loop {
        let chunk = {
            let mut q = PUMP_QUEUE.lock();
            q.pop_front()
        };
        let Some(chunk) = chunk else {
            // Queue empty — park briefly until next enqueue.
            crate::task::sleep_current(500_000);
            continue;
        };
        // Forward to feeding ring. This may park cooperatively when the
        // 16×1024 ring is full (≈80 ms headroom), but the pump is the sole
        // long-term producer so no other task is blocked; enqueuers remain
        // fire-forget up to queue cap. ISR zeroes consumed slots, so after
        // the last chunk drains we play silence — no stale repetition.
        let dev = match DEVICE.get() {
            Some(d) => *d,
            None => continue,
        };
        if let Err(e) = dev.submit_playback(&chunk) {
            SP::puts("[audio] pump submit failed: ");
            SP::puts(e);
            SP::puts("\n");
        }
    }
}

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

/// Play interleaved 16-bit signed stereo PCM at 48 kHz.
///
/// Fire-forget when the pump is alive: samples are queued (copied) and the
/// caller returns immediately; the pump chains them gaplessly through the
/// always-running feeding ring (ISR zero → silence, no repetition). Without a
/// running pump (boot context) this stages synchronously into the ring and
/// may park cooperatively when full.
pub fn play_pcm(samples: &[i16]) -> Result<(), &'static str> {
    // Cheap empty check before allocation.
    if samples.is_empty() {
        return Ok(());
    }
    if samples.len() % 2 != 0 {
        return Err("odd sample count (stereo requires even)");
    }
    if !PUMP_ALIVE.load(Ordering::Acquire) {
        return match DEVICE.get() {
            Some(d) => d.submit_playback(samples),
            None => Err("audio device not initialised"),
        };
    }
    enqueue_playback(samples.to_vec())
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

/// Play a sine tone for `ms` milliseconds.
///
/// Fire-forget when the pump is alive: synthesises the tone into a heap
/// Vec and enqueues it; the pump plays it. Without a pump stages
/// synchronously. Rejects tones longer than 10 seconds to bound allocation
/// (≈1.9 MiB @ 48k stereo). The oscillator phase persists across calls, so
/// back-to-back tones chain without a hard waveform reset (an audible click).
pub fn play_tone(freq_hz: u32, ms: u64) -> Result<(), &'static str> {
    if ms > 10_000 {
        return Err("tone too long (max 10 s)");
    }
    let frames = (SAMPLE_RATE as u64 * ms / 1000) as usize;
    let mut samples = alloc::vec::Vec::with_capacity(frames * CHANNELS);
    samples.resize(frames * CHANNELS, 0i16);
    let step = 2.0 * core::f64::consts::PI * freq_hz as f64 / SAMPLE_RATE as f64;
    let two_pi = 2.0 * core::f64::consts::PI;
    let mut phase = f64::from_bits(TONE_PHASE_BITS.load(Ordering::Relaxed));
    for f in 0..frames {
        let v = (sin_approx(phase) * 0.35 * 32767.0) as i16;
        samples[f * CHANNELS] = v;
        samples[f * CHANNELS + 1] = v;
        phase += step;
    }
    // Keep the accumulator bounded; `sin_approx` folds any angle anyway.
    phase %= two_pi;
    TONE_PHASE_BITS.store(phase.to_bits(), Ordering::Relaxed);
    if !PUMP_ALIVE.load(Ordering::Acquire) {
        return match DEVICE.get() {
            Some(d) => d.submit_playback(&samples),
            None => Err("audio device not initialised"),
        };
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
