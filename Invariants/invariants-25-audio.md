# BedrockOS Invariants — Audio Subsystem & Intel HD Audio Driver

**Version:** 0.5.0
**Date:** 2026-08-15
**Source paths:**
- `kernel/src/audio/mod.rs` — subsystem engine, `AudioDevice` trait, `play_tone`/`play_pcm`, `record_pcm`/`record_pcm_stream`, the playback **pump task** (`enqueue_playback`/`spawn_pump`/`audio_pump_entry`), sine synthesis, `device_name()`/`pub const SAMPLE_RATE, CHANNELS`
- `kernel/src/audio/hda.rs` — Intel HD Audio (ICH6/ICH9) controller driver: reset, CORB/RIRB, serialised verbs, output stream, input stream (capture), the streaming ring (`play_stream`) and the continuous cyclic-ring engine (`play_stream_continuous`)
- `kernel/src/services/universal_timer.rs` — `wait_until_cond_coop` (task-aware cooperative wait)
- `kernel/src/audio/codec.rs` — generic HDA codec driver: probe, widget graph, output/input path discovery, path bring-up
- `kernel/src/lib.rs` — `pub mod audio`, `audio::init()` in `Kernel::run()`, `audio::spawn_pump()` after `task::init()`
- `kernel/src/unispace/provider/driver.rs` — `/driver/audio` unispace device surface (AUD-026..028)
- `run.bat`, `fullrun.bat` — `-audiodev dsound` + `ich9-intel-hda` + `hda-output` + `hda-duplex` QEMU wiring

---

## Scope & Platform

**AUD-000** The subsystem is x86_64-only. `audio::init()` is a no-op on riscv64
(the `virt` machine has no PCI audio device); the rest of the subsystem still
compiles there, but `is_ready()` stays `false`.
- Location: `kernel/src/audio/mod.rs` `init()`, `kernel/src/lib.rs:9`

---

## Subsystem Engine (`kernel/src/audio/mod.rs`)

**AUD-001** `AudioDevice` is the playback capability: `name()` + blocking
`play_pcm(&[i16])` for interleaved 16-bit signed stereo PCM at 48 kHz.  It
also carries the record surface: `can_record() -> bool` (default `false`),
blocking `record_pcm(&mut [i16])` (default `Err("capture not supported")`),
and streaming `record_pcm_stream(total_bytes, entry_bytes, sink)` (default
`Err("capture not supported")`).  Defaults keep playback-only devices and the
riscv64 no-op build compiling without overrides.
- Location: `kernel/src/audio/mod.rs:19-86`

**AUD-002** A single device is held in `Once<&'static dyn AudioDevice>`; the
first controller that initialises successfully wins. `is_ready()` mirrors that
via a release/acquire `AtomicBool`.
- Location: `kernel/src/audio/mod.rs:32-34`

**AUD-003** `init()` must run after `pci::init()`. It scans `pci::devices()`
for class `0x04` / subclass `0x03` (multimedia → audio) and calls
`hda::init(dev)` on each. Failure is non-fatal: a controller that fails is
logged and the scan continues; no controller leaves the subsystem idle.
- Location: `kernel/src/audio/mod.rs:43-84`

**AUD-004** `play_pcm()`/`play_tone()` return `Err("audio device not
initialised")` before any hardware is live. Playback is **asynchronous**: both
synthesise (or copy) their samples and call `enqueue_playback`, which queues
them for the pump task and returns immediately. Only when the pump is not
running (no device, boot context, or spawn failure) do they fall back to the
legacy blocking one-shot `play_pcm`. Nothing on the playback path ever HLTs
the BSP (see AUD-028).

**AUD-005** `play_tone(freq_hz, ms)` synthesises a sine into a heap `Vec<i16>`
(stereo, 48 kHz, 0.35 amplitude) and feeds it to `enqueue_playback`. The sine
uses the Bhaskara I rational approximation (`+ - * /` only) because the kernel
is `no_std` and lacks `f64::sin`; max amplitude error ≈ 1.8%.
- Location: `kernel/src/audio/mod.rs` `play_tone()`

---

## HDA Driver (`kernel/src/audio/hda.rs`)

### Device discovery & MMIO

**AUD-010** `hda::init(dev)` decodes BAR0 via `pci::bar::bar` and maps it with
`dma.map_mmio(base, 0x4000)`. QEMU's controller exposes a 0x4000 BAR (a 0x2000
register window mirrored at 0x2000).
- Location: `kernel/src/audio/hda.rs:283-289`

**AUD-011** DMA allocations come from the shared `KernelDma` pool: one page
each for CORB (256×4 B) and RIRB (256×8 B), one BDL page (32×16 B capacity),
and one contiguous 256 KiB PCM staging buffer. The streaming path additionally
allocates its **own** BDL page and 256 KiB PCM ring buffer (`Inner.sbd_*` /
`Inner.sbuf_*`) so its refill loop can drop the Inner mutex (and yield to the
scheduler) without colliding with capture or one-shot playback sharing the
legacy buffer/BDL. All are held for the driver's lifetime (leaked with the
device).
- Location: `kernel/src/audio/hda.rs:291-305`

### Controller reset & rings

**AUD-012** Controller reset follows the spec: `GCTL.CRST` is active-high for
operational mode (0 = held in reset, 1 = operational), so init asserts reset
(writes 0, which also cold-resets QEMU) then deasserts (writes 1), leaving the
controller out of reset with a 10 ms pause each way. `GCAP` is decoded
(ISS = bits [11:8], OSS = bits [15:12]) and the first output stream base is
derived as `0x80 + ISS*0x20`. QEMU's `ich9-intel-hda` reports `GCAP = 0x4401`
→ ISS=4 → out_base = 0x100.
- Location: `kernel/src/audio/hda.rs:349-373`

**AUD-013** CORB and RIRB base pointers are programmed from physical addresses;
both pointers are reset (0x8000 then 0). CORB is started via `CORBCTL.RUN`
(bit 1 — bit 0 is CMEIE, not run). RIRB is started by setting `RIRBCTL.DMA_EN`
(bit 1; there is no "RIRB run" bit). `RIRBCTL.IRQ_EN` (bit 0) is set as well:
QEMU gates CORB processing on `rirb_count != rirb_cnt` and only resets
`rirb_count` when the guest clears `RIRBSTS.IRQ`, and that bit only latches
when IRQ_EN is set. `RINTCNT` is programmed to 0xFF (must be non-zero or
`intel_hda_corb_run` bails while `rirb_count == rirb_cnt`, since RINTCNT
resets to 0); `codec_verb` clears `RIRBSTS.IRQ` on every command so the gate
never actually stalls the ring. Without DMA_EN in RIRBCTL, QEMU drops every
response.
- Location: `kernel/src/audio/hda.rs:375-394`

**AUD-014** Commands are strictly serialised (`codec_verb`): wait for the CORB
ring to drain (CORBRP == CORBWP), write one verb at index `CORBWP + 1` and
advance `CORBWP` (the controller consumes at `CORBRP + 1`, so the Linux
write-at-next-slot convention is required), wait for the controller to consume
it (`CORBRP == CORBWP`), then wait for a new RIRB response and read the newest
entry at `RIRBWP & 0xff`. Each `GET_PARAM`/verb produces exactly one solicited
response. Timeouts are spin-counted.
- Location: `kernel/src/audio/hda.rs:158-203`

### Codec discovery & configuration

**AUD-015** Codecs are discovered via `STATESTS`: bit `i` set means codec `i` is
present (QEMU sets the bit on reset for every attached codec). Every present
codec address (0–15) is probed generically by `codec::probe`, and the first one
with a usable output path is selected. No NIDs are hardcoded.
- Location: `kernel/src/audio/hda.rs` `init()` (`STATESTS` loop), `kernel/src/audio/codec.rs` `probe()`

**AUD-016** `probe()` reads vendor/subsystem/rev from NID 0, finds the audio
function group via `PARAM_FUNCTION_TYPE`, then walks every widget in the group:
`PARAM_AUDIO_WIDGET_CAP`, `PARAM_PIN_CAP` + the full 32-bit default config
(pins), `PARAM_STREAM` (converters), the amplifier capabilities (inherited from
the AFG unless `WCAP_AMP_OVRD`), and the full connection list
(`PARAM_CONNLIST_LEN` + `GET_CONNECT_LIST`, short 4×8-bit or long 2×16-bit
form). `PARAM_NODE_COUNT` responses are parsed per spec 7.3.4.4: bits 7:0 =
start node, bits 23:16 = node count, bits 15:8 reserved (QEMU returns
`0x00010001` at the root and `0x00020002`/`0x00020004` at the audio function
group). Connection-list range entries (MSB set) are expanded to
`[previous+1 .. value&mask]` so the stored list is the *effective* set — which
is also what `SET_CONNECT_SEL` indices refer to. The output path is found by
walking the connection graph: candidate out-pins scored by default-config
device type (analogue line-out/speaker/headphone first), then a reverse walk
through mixers/selectors to the first Audio Output converter. The input path
(capture flows pin → … → ADC, so the walk goes *upstream* from the ADC's
connection list) is discovered to the first in-capable pin. Both paths are
stored source-first as `Vec<(nid, connection_index)>`, each hop tagged with
the connection-list index it uses toward the source.
- Location: `kernel/src/audio/codec.rs` `probe()`, `read_conns()`, `read_pin_config()`, `find_output_path()`, `reach_converter()`, `path_hops()`, `find_input_path()`, `reach_in_pin()`

**AUD-017** `codec::setup_output` brings the output path up: D0 power on the
AFG and every power-capable node on the path; selectors are routed with
`SET_CONNECT_SEL` to the path's connection index (only when the list has >1
entry); amps are unmuted with gain = the effective capability's `NUM_STEPS`
(DAC/pin output amps at index 0, mixer/selector input amps at the per-hop
index); the out-pin gets `PIN_WIDGET_CONTROL = OUT_EN` (+ `HP_EN` when the
default-config device type is headphone-out, + EAPD if `PINCAP_EAPD`); then
`SET_CONV` (stream tag 1, channel 0) and `SET_STREAM_FORMAT` (`0x11`).
`codec::setup_input` mirrors this for the ADC (tag 2, `INPUT_TAG`): amps
unmuted at the per-hop index, selectors routed, and the in-pin biased with the
highest advertised VREF level.  Input bring-up only runs when the selected
codec exposes an ADC, and only a successful result arms capture.
Verbs are sent through the `VerbSender`
trait implemented by the controller, so the codec module stays
controller-agnostic.
- Location: `kernel/src/audio/codec.rs` `setup_output()`, `setup_input()`, `unmute_amp()`, `pick_vref()`, `set_power_d0()`; `kernel/src/audio/hda.rs` `impl VerbSender for Inner`

### Playback

**AUD-018** `play_pcm` stages samples into the DMA buffer (rejected if larger
than 256 KiB), builds a single-entry BDL `{ addr, 0, len, IOC }`, resets the
stream (`SDnCTL.SRST` set then cleared), programs `BDPL/U`, `LVI=0`, `CBL=len`,
`SDnFMT=0x0011`, then starts DMA with `SDnCTL = tag(1)<<20 | RUN`. The stop
write clears RUN but **preserves the stream tag**: QEMU derives the stream
number from `SDnCTL` on every RUN-bit flip, so clearing the tag would notify
the codec with `stnr=0` and the converter (tag 1) would keep playing the
wrapping BDL forever.

`SDnFMT=0x0011` is the Table 53 stream-format structure for 48 kHz/16-bit/
stereo (TYPE=0 · BASE=0 · MULT=000 · DIV=000 · BITS=001 · CHAN=0001), the
encoding shared by the descriptor and the codec verb.  A previous `0x0A11`
decoded to 32 kHz (MULT=001 · DIV=010) and silently disagreed with the codec's
48 kHz `SET_STREAM_FORMAT`; both directions now use `0x0011`.
`play_pcm`'s `LVI=0` single-entry list is a documented QEMU-tolerant deviation
from the "LVI must be ≥ 1" requirement of 3.3.39; `record_pcm` uses two
descriptors and is fully compliant.
- Location: `kernel/src/audio/hda.rs` `play()`

**AUD-019** Completion is polled: `sleep_ms(duration)`, then a BDL-wrap wait
via `wait_until_cond` (500 ms cap), then a 50 ms drain pause, then `RUN=0`.
LPIB stays in `[0, nbytes)` and wraps to 0 once the single-entry BDL has been
fully consumed, so a wrap (LPIB moving backwards after sampling) is the drain
signal — `LPIB >= nbytes` is never observable. The stream tag in `SDnCTL` (1)
must match the codec's `SET_CONV` stream tag or the codec ignores the stream.

**AUD-020** QEMU-specific correctness notes relied upon by this driver:
- Output streams are identified by register index (index ≥ 4 = output), NOT by
  a direction bit in `SDnCTL`; the first output stream offset is derived from
  GCAP as `0x80 + ISS*0x20` (0x100 for QEMU, which reports `GCAP = 0x4401`).
- The controller consumes the CORB command at index `CORBRP + 1`; commands are
  therefore written at `CORBWP + 1`, never at the current write pointer.
- `RINTCNT` must be non-zero or `intel_hda_corb_run` bails while
  `rirb_count == rirb_cnt` (it resets to 0). The gate is only reset when the
  guest clears `RIRBSTS.IRQ`, which requires `RIRBCTL.IRQ_EN`; `codec_verb`
  clears it per command. `RIRBCTL.DMA_EN` must be set or responses are
  dropped.
- `SRST` does not self-clear on QEMU; the driver writes 1 then 0.
- Stream stops must keep the tag in `SDnCTL` (`intel_hda_set_st_ctl` reads the
  stream number from the *new* `ctl` value on a RUN flip); clearing the tag
  emits a stop for `stnr=0`, which matches no converter.
- The `hda-output` topology is *discovered*, never assumed: the driver probes
  the full widget graph (see AUD-015/-016). QEMU's `hda-output` codec happens
  to be root 0 → AFG 1 → DAC 2 → pin 3, and `hda-duplex` adds ADC 4 → pin 5.
- QEMU codec specifics handled generically in `codec.rs`: vendor id `0x1AF4`;
  `GET_CONFIG_DEFAULT` answers index 0 with the whole 32-bit config — as do
  real codecs (the byte-per-index verbs are `SET_CONFIG_DEFAULT_BYTES_0..3`,
  the write form) — so a single read suffices; `PARAM_PCM` is 16-bit plus the
  full 16–96 kHz rate mask; `SET_POWER_STATE` is a no-op that still responds.
  QEMU codecs expose no selectors, no headphone-out pins and no VREF pin-cap
  bits, so the routing / `HP_EN` / VREF paths are inert there; the only
  observable change is the output amp gain, which drops from a blind `0x7F`
  (127) to the advertised `NUM_STEPS` (`0x4A` = 74). `STATESTS` reflects every
  attached codec.
- Sources: QEMU `hw/audio/intel-hda.c`, `hw/audio/intel-hda-defs.h`,
  `hw/audio/hda-codec.c`, Intel HD Audio spec (RINTCNT/RIRBCTL bit layout,
  connection-list formats, `GET_CONFIG_DEFAULT` indexing)

---
 
### Capture (input stream)
 
**AUD-022** Codec selection is two-way-aware.  The first probed analog-output
codec is no longer blindly kept: among analog-out codecs, one that *also* has
an ADC (`c.adc.is_some()`) is preferred over an analog-out-only one, so with
both `hda-output` (cad 0, DAC-only) and `hda-duplex` (cad 1, DAC+ADC) attached
the driver binds the duplex codec and gains both directions.  Selection order
is: ALC256 (hardcoded path) → analog-out with ADC → analog-out → digital.
Capture is armed only when `codec.adc` exists **and** `codec::setup_input`
succeeds; `<audio cap_ready>` gates every record entry point.
- Location: `kernel/src/audio/hda.rs` `init()` (`duplex` preference), `.cap_ready`
 
**AUD-023** The input stream descriptor lives at `0x80` (register index 0 —
QEMU decides stream direction by descriptor index, index ≥ 4 = output; input
descriptors occupy 0..ISS-1 under the output block).  `Inner.in_base` holds
it; `HDA_IN_BASE` (static, 0 = not armed) publishes it to the ISR.  The ISR
clears **both** streams' `SD_STS.BCIS` (write-1-to-clear deasserts the shared
INTx/MSI line) and counts each stream independently — `HDA_IRQ_COUNT` for
output, `HDA_IN_IRQ_COUNT` for input — so a completion on one stream can never
be counted as the other's.  `INTCTL` lights the input stream's completion bit
alongside the output's only when capture is armed.
- Location: `kernel/src/audio/hda.rs` `hda_irq_handler()`, `setup_stream_interrupt()`
 
**AUD-024** `record_pcm(dest)` is the blocking whole-buffer mirror of `play`,
brought into spec compliance: a **two-entry BDL** (both IOC, split at a
128-byte boundary so both buffer starts honour the 3.6.3 alignment), with
`LVI=1` (3.3.39 requires at least two valid descriptors), a word-aligned
`dest` length, stream reset, `BDPL/U+CBL=len+SDnFMT 0x0011` programmed,
started with `SD_CTL = (2 << 20) | RUN | IOCE` (tag 2 = the codec's input
`SET_CONV`; `IOCE` per 3.3.35 so BCIS can raise real completion interrupts),
allowed the full duration, then both entries' completions consumed (IRQ count
delta or two BCIS poll-clears) before a 50 ms FIFO drain, copied into `dest`,
then stopped **preserving the input tag** (QEMU derives the stream number from
`SDnCTL` on every RUN flip, exactly as for output).  `record_pcm_stream`
mirrors `play_stream`: a fixed-geometry BDL ring of `RING_ENTRIES` `eb`-sized
entries (rejecting non-128-aligned `eb`), CBL = padded total,
refilled/consumed entry-by-entry as input completions arrive; each completed
slot is copied into an **owned** `Vec<i16>` (a ring slot is live DMA memory
being overwritten by the controller) and the final chunk is trimmed to
`total_bytes - (needed-1)*eb`.  The ring slot for completion `c` is `(c-1) mod n`,
safe to read until the controller cycles `n` entries later.  QEMU delivers
silence when no host audio source is configured, so the DMA still advances and
the completion fires; a genuinely stalled input DMA times out with
`"capture stalled"` rather than fabricating samples.
- Location: `kernel/src/audio/hda.rs` `record()`, `record_stream()`
 
**AUD-025** Capture is mutually exclusive with playback: both `play_*` and the
`record_*` entry points hold the same `Inner` mutex for their whole duration,
sharing the staging buffer, BDL page and stream-descriptor programming loops.
Simultaneous full-duplex (record-while-play) requires splitting the mutex per
direction and a second BDL/buffer allocation, and is a follow-up phase; the
independent counters and per-stream BCIS handling make it a clean extension.
 
---

## Unispace Exposure (`/driver/audio`)

**AUD-026** The audio engine is exposed through the `/driver` provider as a
single device object (`kernel/src/unispace/provider/driver.rs`). Its value
schema `AUDIO_STATE` is `struct{ present: bool, name: str, sample_rate: u32,
channels: u32, can_record: bool }` — a live snapshot (`present` mirrors
`audio::is_ready()`, `name` is `device_name()`/`""`, format comes from the
exported `SAMPLE_RATE`/`CHANNELS` consts). Two methods queue playback for the
pump: `:play_tone{freq: u32, ms: u64}` and `:play_pcm{pcm: bytes}`, where
`pcm` is raw little-endian interleaved 16-bit signed stereo (an odd length or
an empty payload is `InvalidArgument`; the former 256 KiB staging limit is
gone — the ring streams any length). Values are converted with
`i16::from_le_bytes`.
- Location: `kernel/src/unispace/provider/driver.rs` (`AudioObject`),
  `kernel/src/audio/mod.rs` (`device_name()`, `pub const SAMPLE_RATE/CHANNELS`)

**AUD-027** Without a live device (`!audio::is_ready()`) both playback methods
return `Unsupported` immediately — never a spin, never a synthesised tone from
an absent engine. Engine failures (`&'static str`) also surface as
`Unsupported`; the wire has no errno vocabulary for them. The object is
`ObjectKind::Device` like `/driver/debugserial`, and every read/method path is
`Result`-only (no panics on device data).

**AUD-028** Playback methods are **non-blocking**: `:play_pcm`/`:play_tone`
enqueue owned samples into the bounded pump queue (`PUMP_QUEUE_CAP = 8`) and
return immediately. The pump task (`audio_pump_entry`, spawned by
`spawn_pump` from `Kernel::run()` right after `task::init()`) pops the queue
and feeds the continuous ring, chaining back-to-back requests with no
stop/start seam. When the queue is full an enqueuing task parks cooperatively
(`sleep_current` slices) until the pump frees a slot — it yields, never
HLTs. Boot-context callers (no pump yet) fall back to the legacy blocking
one-shot path. The `selftest` suite probes both method schemas but never
invokes them from boot context.

## Playback Pump & Continuous Ring

**AUD-029** `play_stream` runs the fixed-total streaming ring on the
**dedicated** stream DMA (`Inner.sbd_*`/`Inner.sbuf_*`) and releases the Inner
mutex for its refill loop; completions are awaited with
`universal_timer::wait_until_cond_coop`, which slice-sleeps as a scheduler
task (yielding to the rest of the system) and HLTs only in boot context (no
current task). The loop only touches the dedicated DMA, the output descriptor
and the atomic completion counters, so a yield — or a concurrent capture on
the input descriptor — cannot corrupt state. `record`/`record_stream` keep the
legacy shared buffer/BDL and their HLT waits (capture is still boot-context /
polled; decoupling it the same way is a follow-up).

**AUD-030** `play_stream_continuous` is the "flow through the ends of time"
path: a full cyclic ring (all `RING_ENTRIES` slots `eb`-sized and IOC, `LVI =
n-1`, `CBL = u32::MAX`), fed from a `next` closure that keeps pulling until it
returns `None`, then stopped the instant the last staged slot completes
(before the DMA can cycle back to replay older audio). The CBL = u32::MAX
deviation is documented QEMU tolerance — QEMU treats CBL as the transfer
budget while the descriptors cycle; a real controller would expect
`CBL = Σ BDL lengths`. The pump's closure chains the next queued request
straight into the running ring, so back-to-back `:play_pcm` calls merge into a
single gapless session.
- Location: `kernel/src/audio/hda.rs` `play_stream_continuous()`,
  `kernel/src/audio/mod.rs` `audio_pump_entry()`

**AUD-031** The `AudioDevice` trait gains `play_pcm_stream_continuous(eb,
next)` with a default that collects the whole feed into one `play_pcm_stream`
pass (gapless, one stop at the end); `HdaAudio` overrides it with the cyclic
ring. The dedicated stream BDL/buffer allocation is the enabling factor for
both the lock-free refill loop and future full-duplex (AUD-025).
 
---
 
## Boot Sequence

**AUD-021** `audio::init()` runs in `Kernel::run()` immediately after the xHCI
init block, before VFS init — it depends only on
`pci::init()` (which precedes it) and on the services (DMA, timer).
A `#[cfg(feature = "selftest")]` capture check right after `audio::init()`
records ~250 ms through `record_pcm_stream` when `can_record()` and logs the
byte count/peak/RMS — a serial proof that the input ring advanced even when
the host audio backend feeds silence.
- Location: `kernel/src/lib.rs` `run()`

---

## Design Notes

- The driver is deliberately polled; HDA MSI (`pci::msi::enable` +
  `services.msi`) is a clean follow-up since the QEMU controller supports it.
- The fixed 48 kHz / 16-bit / stereo stream format matches both the controller
  `SDnFMT` encoding and the codec format verb — both are the Table 53 stream
  format structure, value `0x11`/`0x0011` (the old `0x0A11` descriptor value
  was a 32 kHz encoding and is fixed); a future
  rate/channel change must keep the two in agreement.
- Playback is fully decoupled from callers: the pump task + continuous ring
  give gapless, non-blocking output that chains queued requests with no seam,
  and cooperative waits keep the rest of the system flowing while the DMA
  rides.  Capture remains blocking/polled; the same decoupling (its own DMA +
  a cooperative completion loop) is the natural next phase, and the dedicated
  per-direction DMA now removes the AUD-025 blocker for true full-duplex.
  The streaming ring (`play_pcm_stream` / `record_pcm_stream`) already uses
  IOC completions end-to-end; what remains is capture-side decoupling and HDA
  MSI (`pci::msi::enable` + `services.msi`), a clean follow-up since the QEMU
  controller supports it.
