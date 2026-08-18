# BedrockOS Invariants — Audio Subsystem & Intel HD Audio Driver

**Version:** 0.8.0
**Date:** 2026-08-18
**Source paths:**
- `kernel/src/audio/mod.rs` — subsystem engine, `AudioDevice` trait, `submit_playback`/`read_capture`, `play_tone`/`play_pcm`/`record_pcm`, sine synthesis, `device_name()`/`pub const SAMPLE_RATE, CHANNELS`
- `kernel/src/audio/hda.rs` — Intel HD Audio (ICH6/ICH9) controller driver: reset, CORB/RIRB, serialised verbs, per-direction **feeding rings**, LPIB-reconciled cursor advance + slot zeroing
- `kernel/src/services/universal_timer.rs` — `wait_until_cond_coop` (task-aware cooperative wait)
- `kernel/src/audio/codec.rs` — generic HDA codec driver: probe, widget graph, output/input path discovery, path bring-up
- `kernel/src/lib.rs` — `pub mod audio`, `audio::init()` in `Kernel::run()`
- `kernel/src/unispace/provider/driver.rs` — `/driver/audio` unispace device surface (AUD-026..028)
- `run.bat`, `fullrun.bat` — `-audiodev dsound` + `ich9-intel-hda` + `hda-output` + `hda-duplex` QEMU wiring

---

## Scope & Platform

**AUD-000** The subsystem is x86_64-only. `audio::init()` is a no-op on riscv64
(the `virt` machine has no PCI audio device); the rest of the subsystem still
compiles there, but `is_ready()` stays `false`.
- Location: `kernel/src/audio/mod.rs` `init()`, `kernel/src/lib.rs`

---

## Subsystem Engine (`kernel/src/audio/mod.rs`)

**AUD-001** `AudioDevice` is the playback/capture capability: `name()` +
`submit_playback(&[i16])` for interleaved 16-bit signed stereo PCM at 48 kHz,
`can_record() -> bool` (default `false`), and `read_capture(&mut [i16])`
(default `Err("capture not supported")`).  Defaults keep playback-only devices
and the riscv64 no-op build compiling without overrides.
- Location: `kernel/src/audio/mod.rs`

**AUD-002** A single device is held in `Once<&'static dyn AudioDevice>`; the
first controller that initialises successfully wins. `is_ready()` mirrors that
via a release/acquire `AtomicBool`.

**AUD-003** `init()` must run after `pci::init()`. It scans `pci::devices()`
for class `0x04` / subclass `0x03` (multimedia → audio) and calls
`hda::init(dev)` on each. Failure is non-fatal: a controller that fails is
logged and the scan continues; no controller leaves the subsystem idle.

**AUD-004** `play_pcm()`/`record_pcm()` return `Err("audio device not
initialised")` before any hardware is live.  Both are **feeding-ring** calls:
they stage/read the next DMA slot and return once done, parking the caller
(cooperatively in task context, HLT in boot context) only when the ring is
momentarily full/empty.  Nothing on the playback path ever HLTs the BSP when a
task context exists (see AUD-028).

**AUD-005** `play_tone(freq_hz, ms)` synthesises a sine into a heap `Vec<i16>`
(stereo, 48 kHz, 0.35 amplitude) and feeds it to `submit_playback`. The sine
uses the Bhaskara I rational approximation (`+ - * /` only) because the kernel
is `no_std` and lacks `f64::sin`; max amplitude error ≈ 1.8%.

**AUD-006** There is **no kernel pump task** and **no request queue**.  The
DMA rings run permanently and are fed synchronously by callers.  The old
`enqueue_playback`/`spawn_pump`/`audio_pump_entry`/`PUMP_QUEUE`/`PUMP_SESSIONS`
machinery and the `play_pcm_stream`/`play_pcm_stream_continuous`/
`record_pcm_stream` streaming APIs are **removed**; streaming a long buffer is
a caller loop over `submit_playback`/`read_capture`.

---

## HDA Driver (`kernel/src/audio/hda.rs`)

### Feeding rings (AUD-010…AUD-014 cover discovery)

**AUD-015** Each direction owns one fixed-geometry cyclic BDL ring, programmed
**once at init** (`Inner::start_ring`): `RING_SLOTS` (= 8) IOC entries of
`RING_SLOT_BYTES` (= 2048 B, ≈ 10.7 ms) over a contiguous `RING_BUF_BYTES`
buffer, `LVI = RING_SLOTS-1`, `CBL = RING_SLOTS * RING_SLOT_BYTES`, then DMA
started with `RUN | IOCE` and left running for the kernel's lifetime.  There is
no per-call stream reset or descriptor reprogramming.

**AUD-016** Ring ownership is two **byte** cursors per direction, both lock-free
atomics (the DMA works in fixed BDL slots, but a partial slot simply carries
over into the next call — no data is lost, no silence is inserted mid-slot):
- Playback: `OUT_PRODUCED` (bytes staged by callers) and `OUT_COMPLETED`
  (bytes the DMA has finished playing).  A caller writes at
  `OUT_PRODUCED % RING_BUF_BYTES` while the staged-but-unplayed fill level
  `OUT_PRODUCED - OUT_COMPLETED` stays below `MAX_STAGED_BYTES` =
  `RING_BUF_BYTES - RING_SLOT_BYTES` (7 slots ≈ 74 ms).  `OUT_COMPLETED`
  tracks the exact byte the DMA has played, so the write head always stays at
  or ahead of the play head.  Only a **true underrun** — the DMA caught up to
  or passed the write head (`OUT_PRODUCED <= OUT_COMPLETED`), a cold start or
  a stall longer than the staged window — realigns `OUT_PRODUCED` to
  `c + 2·slot` (the unwritten gap plays as zeroed silence).  A partial fill
  (`0 < OUT_PRODUCED - OUT_COMPLETED < cap`) is healthy and never touches
  `OUT_PRODUCED`; the stall indicator is **never** the fill level dropping
  below two slots.  When the staging cap is reached the caller parks
  cooperatively until a completion frees space.
- Capture: `IN_CAPTURED` (bytes the input DMA has filled, reconciled against
  the input `SDnLPIB`) and `IN_CONSUMED` (bytes read by callers); a caller
  reads at `IN_CONSUMED % RING_BUF_BYTES` whenever
  `IN_CAPTURED.wrapping_sub(IN_CONSUMED) > 0`, never reading past
  `IN_CAPTURED` (so it never touches the slot the input DMA is filling).  If
  the caller lags a full ring behind the DMA, the overrun is detected and
  `IN_CONSUMED` is realigned to the newest captured slot, dropping the
  overwritten tail instead of replaying stale data.

**AUD-017** The completion ISR (`hda_irq_handler`) is the only asynchronous
actor.  Cursors are advanced by **reconciling against `SDnLPIB`**, never
counted per interrupt: the controller latches BCIS **sticky-once** even when
several slots completed between services, so a blind `+1 slot per BCIS` would
let `OUT_COMPLETED`/`IN_CAPTURED` lag the DMA permanently (QEMU updates LPIB
byte-by-byte on every xfer and resets it to 0 at BDL wrap).  The servicer reads
LPIB and advances the cursor by the **exact byte distance**
`(lpib + ring - last_lpib) % ring` since the previous observation (tracked in
`LAST_OUT_LPIB`/`LAST_IN_LPIB`), so a wrap is native and no absolute-offset
wrap guessing can fabricate a false full-ring lap (which would teleport the
cursor ~7 slots ahead and wipe the buffer).  Every just-consumed **output
byte** is zeroed (before the `Release` store of the cursor), so a stalled
producer plays silence rather than a stale tail from an earlier lap.  Input
bytes are not zeroed — the DMA overwrites its own ring.  The producer's
`Acquire` load of `OUT_COMPLETED` synchronises with the servicer's `Release`
store, so the zeroing is always visible before the producer reclaims that byte —
there is no double-write race (the producer only ever writes at or ahead of the
just-consumed position).

**AUD-018** If no interrupt route is established, `INTERRUPT_DRIVEN` stays
false and the waiting caller services the BCIS latches itself
(`service_polled_completions`), reconciling the cursors against LPIB exactly as
the ISR would — unconditionally each poll, since one latch may cover several
slots and LPIB is a no-op when no boundary was crossed.  Because it runs only
when the ISR is not wired, there is no double-count race.

**AUD-019** Callers park via `ring_wait_until`, which loops
`wait_until_cond_coop` (task context: cooperative `sleep_current` slices, never
HLTing the BSP; boot context: HLT) with a 10 ms re-check window.

**AUD-020** Runtime playback/capture never take the `Inner` mutex: the rings,
buffers and cursors are lock-free statics.  `Inner` (and its mutex) is held
only during init/bring-up (verb serialisation).  This is what makes playback,
capture and full-duplex coexist cleanly (removes the old AUD-025 blocker).

### Discovery & configuration (unchanged from 0.6.0)

**AUD-021** `hda::init(dev)` decodes BAR0 via `pci::bar::bar` and maps it with
`dma.map_mmio(base, 0x4000)`.  DMA: one page each for CORB (256×4 B) and RIRB
(256×8 B); per direction a BDL page and a contiguous `RING_BUF_BYTES` buffer.
All held for the driver's lifetime.

**AUD-022** Controller reset, GCAP decode (`out_base = 0x80 + ISS*0x20`),
CORB/RIRB programming (`CORBCTL.RUN`, `RIRBCTL.DMA_EN|IRQ_EN`, `RINTCNT=0xFF`),
serialised verbs (`codec_verb`, Linux write-at-`CORBWP+1` convention) are
unchanged from 0.6.0.  Commands are strictly serialised; `drain_rirb` keeps the
ring in sync across unsolicited responses.

**AUD-023** Codec discovery/selection and path bring-up live in `codec.rs`,
unchanged: `probe()` walks the widget graph, `find_output_path`/`find_input_path`
discover converter↔pin paths, `setup_output`/`setup_input`/`setup_alc256_output`
bring them up; ALC256 hardcoded path preferred when the widget walk truncates.
Capture is armed (`cap_ready`) only when the codec has an ADC **and**
`setup_input` succeeds.  The input ring is started only then.

### Interrupts

**AUD-024** `setup_stream_interrupt` enables MSI (preferred) or legacy INTx
for the output (and, when armed, input) stream completions and sets
`INTERRUPT_DRIVEN`.  A failed route (no vector / no INTx) leaves
`INTERRUPT_DRIVEN` false and the polled fallback (AUD-018) takes over.

---

## Unispace Exposure (`/driver/audio`)

**AUD-026** The audio engine is exposed through the `/driver` provider as a
single device object. Its value schema `AUDIO_STATE` is `struct{ present: bool,
name: str, sample_rate: u32, channels: u32, can_record: bool }`. Two methods
feed the running ring: `:play_tone{freq: u32, ms: u64}` and
`:play_pcm{pcm: bytes}` (raw little-endian interleaved 16-bit signed stereo).
`pcm` is converted with `i16::from_le_bytes`; an empty or odd-length payload is
`InvalidArgument`.

**AUD-027** Without a live device (`!audio::is_ready()`) both playback methods
return `Unsupported` immediately. Engine failures (`&'static str`) also surface
as `Unsupported`. The object is `ObjectKind::Device` and every read/method path
is `Result`-only.

**AUD-028** Playback methods are **synchronous feeding-ring** calls: they stage
the samples into the running DMA ring and return once staged, parking the
caller cooperatively only when the ring is momentarily full.  The DMA ring
never stops, so back-to-back requests chain gaplessly with no seam.  In task
context the caller yields (`sleep_current` slices), never HLTs the BSP.

---

## Boot Sequence

**AUD-021** `audio::init()` runs in `Kernel::run()` immediately after the xHCI
init block, before VFS init.  A `#[cfg(feature = "selftest")]` capture check
right after `audio::init()` records ~250 ms through `record_pcm` in 4096-sample
blocks when `can_record()` and logs the byte count/peak/RMS.

---

## Design Notes

- The driver is polled/IRQ hybrid: MSI/INTx when available, BCIS polling as a
  safe fallback.  No kernel audio task exists — the rings are fed by callers.
- The fixed 48 kHz / 16-bit / stereo stream format matches the controller
  `SDnFMT` and the codec format verb (`0x11`/`0x0011`); a future rate/channel
  change must keep the two in agreement.
- `RING_SLOT_BYTES` (DMA-quantum granularity, ≈ 10.7 ms), `RING_SLOTS`
  (in-flight headroom, ≈ 85 ms), and the staging cap `MAX_STAGED_BYTES`
  (≈ 74 ms of in-flight audio) are tunable constants; they keep the 128-byte
  BDL buffer alignment of HDA 3.6.3.  The staging cap must stay a multiple of
  `RING_SLOT_BYTES` and strictly inside `RING_BUF_BYTES`.
- Full-duplex works: separate output/input rings and cursors, no shared mutex
  at runtime.
- Playback realigns (a genuine stall: DMA lapped the producer) and capture
  overruns (consumer lags a full ring) each emit a single serial log line.
  They indicate a real stall, never normal operation — a healthy producer
  keeps ahead of the play head and a healthy consumer drains before the DMA
  wraps, so steady-state playback is completely silent on the serial console.