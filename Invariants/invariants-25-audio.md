# BedrockOS Invariants — Audio Subsystem & Intel HD Audio Driver

**Version:** 0.1.0
**Date:** 2026-08-02
**Source paths:**
- `kernel/src/audio/mod.rs` — subsystem engine, `AudioDevice` trait, `play_tone`/`play_pcm`, sine synthesis
- `kernel/src/audio/hda.rs` — Intel HD Audio (ICH6/ICH9) controller driver: reset, CORB/RIRB, serialised verbs, output stream
- `kernel/src/audio/codec.rs` — generic HDA codec driver: probe, widget graph, output/input path discovery, path bring-up
- `kernel/src/module/audio_test.rs` — `AudioTest` boot module (melody smoke test)
- `kernel/src/lib.rs` — `pub mod audio`, `audio::init()` in `Kernel::run()`
- `run.bat`, `fullrun.bat` — `-audiodev dsound` + `ich9-intel-hda` + `hda-output` + `hda-duplex` QEMU wiring

---

## Scope & Platform

**AUD-000** The subsystem is x86_64-only. `audio::init()` is a no-op on riscv64
(the `virt` machine has no PCI audio device); the rest of the module still
compiles there, but `is_ready()` stays `false`.
- Location: `kernel/src/audio/mod.rs` `init()`, `kernel/src/lib.rs:9`

---

## Subsystem Engine (`kernel/src/audio/mod.rs`)

**AUD-001** `AudioDevice` is the playback capability: `name()` + blocking
`play_pcm(&[i16])` for interleaved 16-bit signed stereo PCM at 48 kHz.
- Location: `kernel/src/audio/mod.rs:20-27`

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
initialised")` before any hardware is live. Playback is synchronous and
blocking; it is only invoked from the boot-module context (serialised).

**AUD-005** `play_tone(freq_hz, ms)` synthesises a sine into a heap `Vec<i16>`
(stereo, 48 kHz, 0.35 amplitude) and feeds it to `play_pcm`. The sine uses the
Bhaskara I rational approximation (`+ - * /` only) because the kernel is
`no_std` and lacks `f64::sin`; max amplitude error ≈ 1.8%.
- Location: `kernel/src/audio/mod.rs:88-125`

---

## HDA Driver (`kernel/src/audio/hda.rs`)

### Device discovery & MMIO

**AUD-010** `hda::init(dev)` decodes BAR0 via `pci::bar::bar` and maps it with
`dma.map_mmio(base, 0x4000)`. QEMU's controller exposes a 0x4000 BAR (a 0x2000
register window mirrored at 0x2000).
- Location: `kernel/src/audio/hda.rs:283-289`

**AUD-011** DMA allocations come from the shared `KernelDma` pool: one page
each for CORB (256×4 B) and RIRB (256×8 B), one BDL page (32×16 B capacity),
and one contiguous 256 KiB PCM staging buffer. All are held for the driver's
lifetime (leaked with the device).
- Location: `kernel/src/audio/hda.rs:291-295`

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
highest advertised VREF level. Capture is not exposed by the audio engine yet,
so input bring-up is best-effort. Verbs are sent through the `VerbSender`
trait implemented by the controller, so the codec module stays
controller-agnostic.
- Location: `kernel/src/audio/codec.rs` `setup_output()`, `setup_input()`, `unmute_amp()`, `pick_vref()`, `set_power_d0()`; `kernel/src/audio/hda.rs` `impl VerbSender for Inner`

### Playback

**AUD-018** `play_pcm` stages samples into the DMA buffer (rejected if larger
than 256 KiB), builds a single-entry BDL `{ addr, 0, len, IOC }`, resets the
stream (`SDnCTL.SRST` set then cleared), programs `BDPL/U`, `LVI=0`, `CBL=len`,
`SDnFMT=0x0A11`, then starts DMA with `SDnCTL = tag(1)<<20 | RUN`. The stop
write clears RUN but **preserves the stream tag**: QEMU derives the stream
number from `SDnCTL` on every RUN-bit flip, so clearing the tag would notify
the codec with `stnr=0` and the converter (tag 1) would keep playing the
wrapping BDL forever.
- Location: `kernel/src/audio/hda.rs:248-299`

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

## Boot Sequence

**AUD-021** `audio::init()` runs in `Kernel::run()` immediately after the xHCI
init block, before VFS init and module tests — it depends only on
`pci::init()` (which precedes it) and on the services (DMA, timer).
- Location: `kernel/src/lib.rs` `run()`

**AUD-022** `AudioTest` sits before `InputTest` in the x86_64 module list. It
SKIPs (reports OK) when no audio device is present and returns an error on
playback failure, matching the module-test convention.
- Location: `kernel/src/module/registry.rs:38-47`, `kernel/src/module/audio_test.rs`

---

## Design Notes

- The driver is deliberately polled; HDA MSI (`pci::msi::enable` +
  `services.msi`) is a clean follow-up since the QEMU controller supports it.
- The fixed 48 kHz / 16-bit / stereo stream format matches both the controller
  `SDnFMT` encoding (`0x0A11`) and the codec format verb (`0x11`); a future
  rate/channel change must keep the two in agreement.
- Playback is blocking by design for now; a ring of buffers + interrupt-driven
  completion (BDL `IOC`, `SDnSTS.BCIS`) is the path to non-blocking audio.
