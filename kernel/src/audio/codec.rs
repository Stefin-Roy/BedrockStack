//! Generic Intel HD Audio codec driver.
//!
//! The HDA controller (see `hda.rs`) is codec-agnostic: it only moves verbs
//! over the CORB/RIRB rings.  This module speaks the codec side of the Intel
//! HD Audio spec (Chapter 7) against whatever codec is attached:
//!
//!   - probe a codec address: read vendor/subsystem/rev, find the audio
//!     function group, walk every widget with `PARAM_AUDIO_WIDGET_CAP`,
//!     `PARAM_PIN_CAP`, `PARAM_PCM`, the amplifier capabilities (inherited
//!     from the function group unless `WCAP_AMP_OVRD`) and the connection
//!     list (range entries expanded);
//!   - discover an output path (`DAC -> [mixer/selector] -> pin`) and an input
//!     path (`pin -> [mixer/selector] -> ADC`) by walking the connection
//!     graph — never by hardcoding node numbers (OEMs shuffle them).  Each hop
//!     records the connection index it uses toward the source;
//!   - bring a path up: power D0 on every node, select the routed inputs on
//!     selectors, unmute the relevant amps at their per-hop connection index
//!     (gain = the amp capability's step count), enable the pin (HP_EN for
//!     headphone jacks, VREF where advertised), then bind the converter to a
//!     stream tag + format.
//!
//! It is written against the Intel HD Audio 1.0a spec and verified against
//! QEMU's `hda-output` and `hda-duplex` codecs (`hw/audio/hda-codec.c`,
//! `hw/audio/hda-codec-common.h`).  QEMU quirks that must not be relied upon
//! as "real" behaviour are flagged inline.

use crate::drivers::serial::SerialPort;
use alloc::vec::Vec;

// ── Widget types (7.3.4.5) ──────────────────────────────────────────
pub const WIDGET_AUD_OUT: u32 = 0x0;
pub const WIDGET_AUD_IN: u32 = 0x1;
pub const WIDGET_MIXER: u32 = 0x2;
pub const WIDGET_SEL: u32 = 0x3;
pub const WIDGET_PIN: u32 = 0x4;
pub const WIDGET_POWER: u32 = 0x5;
pub const WIDGET_VOL_KNOB: u32 = 0x6;
pub const WIDGET_BEEP: u32 = 0x7;
pub const WIDGET_VENDOR: u32 = 0xF;

// ── Parameter ids (7.3.4) ───────────────────────────────────────────
pub const PARAM_VENDOR_ID: u32 = 0x00;
pub const PARAM_SUBSYSTEM_ID: u32 = 0x01;
pub const PARAM_REV_ID: u32 = 0x02;
pub const PARAM_NODE_COUNT: u32 = 0x04;
pub const PARAM_FUNCTION_TYPE: u32 = 0x05;
pub const PARAM_AUDIO_FG_CAP: u32 = 0x08;
pub const PARAM_AUDIO_WIDGET_CAP: u32 = 0x09;
pub const PARAM_PCM: u32 = 0x0A;
pub const PARAM_STREAM: u32 = 0x0B;
pub const PARAM_PIN_CAP: u32 = 0x0C;
pub const PARAM_AMP_IN_CAP: u32 = 0x0D;
pub const PARAM_CONNLIST_LEN: u32 = 0x0E;
pub const PARAM_POWER_STATE: u32 = 0x0F;
pub const PARAM_GPIO_CAP: u32 = 0x11;
pub const PARAM_AMP_OUT_CAP: u32 = 0x12;

// ── Get verbs (7.3.3) ───────────────────────────────────────────────
pub const VERB_GET_PARAM: u32 = 0xF00;
pub const VERB_GET_CONNECT_SEL: u32 = 0xF01;
pub const VERB_GET_CONNECT_LIST: u32 = 0xF02;
pub const VERB_GET_POWER_STATE: u32 = 0xF05;
pub const VERB_GET_CONV: u32 = 0xF06;
pub const VERB_GET_PIN_WIDGET_CONTROL: u32 = 0xF07;
pub const VERB_GET_CONFIG_DEFAULT: u32 = 0xF1C;
pub const VERB_GET_AMP_GAIN_MUTE: u32 = 0x0B00;
/// Vendor processing-coefficient read (same opcode as `VERB_SET_COEF_INDEX`;
/// after the index is selected this reads the value back).
pub const VERB_GET_PROC_COEF: u32 = 0x500;

// ── Set verbs (7.3.3) ───────────────────────────────────────────────
pub const VERB_SET_STREAM_FORMAT: u32 = 0x200;
pub const VERB_SET_AMP_GAIN_MUTE: u32 = 0x300;
/// Vendor processing-coefficient index select (8-bit payload).
pub const VERB_SET_COEF_INDEX: u32 = 0x500;
/// Vendor processing-coefficient write (16-bit payload — see `verb()`).
pub const VERB_SET_PROC_COEF: u32 = 0x600;
pub const VERB_SET_CONNECT_SEL: u32 = 0x701;
pub const VERB_SET_POWER_STATE: u32 = 0x705;
pub const VERB_SET_CONV: u32 = 0x706;
pub const VERB_SET_PIN_WIDGET_CONTROL: u32 = 0x707;
pub const VERB_SET_EAPD_BTLENABLE: u32 = 0x70C;
pub const VERB_SET_CODEC_RESET: u32 = 0x7FF;

// ── Audio Widget Capabilities (7.3.4.6) ─────────────────────────────
pub const WCAP_STEREO: u32 = 1 << 0;
pub const WCAP_IN_AMP: u32 = 1 << 1;
pub const WCAP_OUT_AMP: u32 = 1 << 2;
pub const WCAP_AMP_OVRD: u32 = 1 << 3;
pub const WCAP_FORMAT_OVRD: u32 = 1 << 4;
pub const WCAP_CONN_LIST: u32 = 1 << 8;
pub const WCAP_POWER: u32 = 1 << 10;
pub const WCAP_TYPE_MASK: u32 = 0xF << 20;
pub const WCAP_TYPE_SHIFT: u32 = 20;

// ── Supported PCM formats (7.3.4.7) ─────────────────────────────────
/// Bit 6 of the rates field = 48 kHz ("must be supported by all codecs").
pub const PCM_RATE_48K: u32 = 1 << 6;
/// Bit 20 of the formats field = 16-bit.
pub const PCM_BITS_16: u32 = 1 << 20;

// ── Pin Capabilities (7.3.4.9) ──────────────────────────────────────
pub const PINCAP_OUT: u32 = 1 << 4;
pub const PINCAP_IN: u32 = 1 << 5;
/// Any VREF support: bit 8 (Vref present), bit 12 (50%), bit 13 (80%/100%).
pub const PINCAP_VREF_MASK: u32 = 0x37 << 8;
pub const PINCAP_EAPD: u32 = 1 << 16;

// ── Amp verbs / caps ────────────────────────────────────────────────
pub const AMP_SET_OUTPUT: u32 = 1 << 15;
pub const AMP_SET_INPUT: u32 = 1 << 14;
pub const AMP_SET_LEFT: u32 = 1 << 13;
pub const AMP_SET_RIGHT: u32 = 1 << 12;
pub const AMP_INDEX_MASK: u32 = 0xF << 8;
pub const AMP_MUTE: u32 = 1 << 7;
pub const AMP_GAIN_MASK: u32 = 0x7F;
/// Amp capability: mute supported (bit 31), number of gain steps (bits 14:8).
pub const AMPCAP_MUTE: u32 = 1 << 31;
pub const AMPCAP_NUM_STEPS: u32 = 0x7F << 8;

// ── Pin Widget Control (7.3.3.31) ───────────────────────────────────
pub const PIN_OUT_EN: u32 = 0x40;
pub const PIN_IN_EN: u32 = 0x20;
pub const PIN_HP_EN: u32 = 0x80;
pub const PIN_VREF_50: u32 = 0x10;
pub const PIN_VREF_80: u32 = 0x11;
pub const PIN_VREF_100: u32 = 0x12;
pub const PIN_VREF_GRD: u32 = 0x13;

// ── Default-config device types (7.3.4.11) ──────────────────────────
/// Headphone-out jack (default-config device field).
pub const JACK_HP_OUT: u32 = 2;

// ── EAPD/BTL ────────────────────────────────────────────────────────
pub const EAPD_EN: u32 = 1 << 1;

// ── Realtek ALC vendor coefficients (node 0x20) ─────────────────────
/// Node on which Realtek exposes its vendor coefficients.
const RTL_COEF_NID: u32 = 0x20;
/// COEF 0x10 bit 9 — "Change EAPD to verb control".  Until this is cleared,
/// `VERB_SET_EAPD_BTLENABLE` is ignored and the speaker amplifier stays off
/// (Linux `alc_fill_eapd_coef`).
const ALC_EAPD_COEF: u32 = 0x10;
const ALC_EAPD_VERB_CONTROL: u32 = 9;

/// Clear `bit` of vendor COEF `idx` on node 0x20 (read-modify-write, with
/// the index re-selected before each access as Linux does).
fn alc_coef_clear_bit(s: &mut dyn VerbSender, c: &Codec, idx: u32, bit: u32) {
    let _ = s.verb(c.cad, RTL_COEF_NID, VERB_SET_COEF_INDEX, idx);
    let old = s
        .verb(c.cad, RTL_COEF_NID, VERB_GET_PROC_COEF, 0)
        .unwrap_or(0);
    let _ = s.verb(c.cad, RTL_COEF_NID, VERB_SET_COEF_INDEX, idx);
    let _ = s.verb(c.cad, RTL_COEF_NID, VERB_SET_PROC_COEF, old & !(1 << bit));
}

/// ALC269-family EAPD quirk: clear COEF 0x10 bit 9 so `VERB_SET_EAPD_BTLENABLE`
/// actually drives the pin EAPD.  Mirrors the vendor list in Linux
/// `alc_fill_eapd_coef` (the `0x10, 1<<9, 0` case, incl. ALC256 = 0x10ec0256).
/// No-op on other codecs (and on QEMU's, which has no coef widget semantics).
fn realtek_eapd_verb_control(s: &mut dyn VerbSender, c: &Codec) {
    if matches!(
        c.vendor,
        0x10ec0230
            | 0x10ec0233
            | 0x10ec0235
            | 0x10ec0236
            | 0x10ec0245
            | 0x10ec0255
            | 0x10ec0256
            | 0x19e58326
            | 0x10ec0257
            | 0x10ec0282
            | 0x10ec0283
            | 0x10ec0286
            | 0x10ec0288
            | 0x10ec0298
            | 0x10ec0300
    ) {
        alc_coef_clear_bit(s, c, ALC_EAPD_COEF, ALC_EAPD_VERB_CONTROL);
    }
}

// ── Realtek ALC256 hardcoded output nids (ALC269-family wiring) ─────
/// Audio output converter (DAC) of the ALC256.
const RTL_ALC256_DAC: u32 = 0x02;
/// Internal speaker output pin.
const RTL_ALC256_SPKR_PIN: u32 = 0x14;
/// Headphone-jack output pin.
const RTL_ALC256_HP_PIN: u32 = 0x21;

/// True for the Realtek ALC256 codec.  On real hardware its audio function
/// group can report a truncated node count (the count byte reads 2 instead of
/// 32) so the generic widget walk never reaches the DAC/pins; the driver then
/// binds the hardcoded analog path below instead of falling back to a digital
/// (HDMI) function that has no path to the speakers.
pub fn is_realtek_alc256(vendor: u32) -> bool {
    vendor == 0x10ec0256
}

// ── Power state values ──────────────────────────────────────────────
pub const PWR_D0: u32 = 0;

// ── Function group types ────────────────────────────────────────────
pub const GRP_AUDIO_FUNCTION: u32 = 0x01;

/// Maximum connection-list length per the spec (`HDA_MAX_CONNECTIONS`).
pub const MAX_CONNS: usize = 32;
/// The stream format this driver negotiates: PCM, 16-bit, stereo, 48 kHz.
/// `0x11` = base 48 kHz, div 1, mult 1, 16 bits, 2 channels.
pub const STREAM_FMT_48K_STEREO_16: u16 = 0x11;

/// Send one verb to a codec and return the solicited response.
///
/// Implemented by the controller (`hda::Inner`) which owns the CORB/RIRB
/// rings.  Commands are strictly serialised.
pub trait VerbSender {
    fn verb(&mut self, cad: u32, nid: u32, v: u32, payload: u32) -> Result<u32, &'static str>;
}

/// Assemble an HDA command.
///
/// Bits `[31:28]` codec address, `[27:20]` node id, then the verb+payload.
/// Two forms exist: 12-bit verb + 8-bit payload (all `0x7xx`/`0xFxx` verbs)
/// and 4-bit verb + 16-bit payload (`SET_STREAM_FORMAT`, `SET_AMP_GAIN_MUTE`,
/// `GET_AMP_GAIN_MUTE`).
pub fn verb(cad: u32, nid: u32, v: u32, payload: u32) -> u32 {
    if v == VERB_SET_STREAM_FORMAT
        || v == VERB_SET_AMP_GAIN_MUTE
        || v == VERB_GET_AMP_GAIN_MUTE
        || v == VERB_SET_PROC_COEF
    {
        // 4-bit verb + 16-bit payload: the verb's high nibble goes in bits
        // 19:16 and the full data in bits 15:0.  `SET_PROC_COEF` carries a
        // 16-bit coefficient value (Linux encodes it identically: `verb<<8 |
        // param`, and 0x600's low nibble is 0, so both forms agree).
        (cad << 28) | (nid << 20) | ((v & 0xF00) << 8) | (payload & 0xFFFF)
    } else {
        (cad << 28) | (nid << 20) | (v << 8) | (payload & 0xFF)
    }
}

/// A widget discovered on the codec.
#[derive(Clone, Copy)]
pub struct Widget {
    pub nid: u32,
    pub wcap: u32,
    pub pincap: u32,
    pub stream: u32,
    pub conns: [u32; MAX_CONNS],
    pub nconns: u32,
    /// Effective amplifier capabilities: the widget's own when `WCAP_AMP_OVRD`
    /// is set, else inherited from the audio function group.
    pub amp_in: u32,
    pub amp_out: u32,
    /// Full 32-bit default pin configuration (pins only).
    pub pin_cfg: u32,
}

impl Widget {
    pub fn wtype(&self) -> u32 {
        (self.wcap & WCAP_TYPE_MASK) >> WCAP_TYPE_SHIFT
    }
}

/// Everything learned about one codec.
pub struct Codec {
    pub cad: u32,
    pub vendor: u32,
    pub subsystem: u32,
    pub rev: u32,
    /// Audio function group nid (owns the widget range).
    pub fg: u32,
    /// Raw `PARAM_NODE_COUNT` response of the function group (start + count
    /// packed in the low and high bytes).  Diagnostic: `widgets.len()` shows
    /// how many widgets actually answered, so a truncated walk is visible.
    pub wnc: u32,
    /// Negotiated converter stream format (always `STREAM_FMT_48K_STEREO_16`).
    pub fmt: u16,
    pub dac: Option<u32>,
    pub out_pin: Option<u32>,
    /// `dac .. out_pin` — every node on the output path, inclusive.  Each hop
    /// carries the connection-list index the path uses (toward the source).
    pub out_path: Vec<(u32, usize)>,
    pub adc: Option<u32>,
    pub in_pin: Option<u32>,
    /// `in_pin .. adc` — every node on the input path, inclusive, with the
    /// connection-list index each hop uses (toward the source pin).
    pub in_path: Vec<(u32, usize)>,
    pub widgets: Vec<Widget>,
}

impl Codec {
    fn widget(&self, nid: u32) -> Option<&Widget> {
        self.widgets.iter().find(|w| w.nid == nid)
    }

    /// True when the discovered output pin's default-config device type is an
    /// analog output (line-out, speaker, headphone) rather than digital.
    /// Used to prefer a codec that can actually reach the speakers.
    pub fn out_is_analog(&self) -> bool {
        match self.out_pin {
            Some(pin) => self
                .widget(pin)
                .map_or(false, |w| matches!((w.pin_cfg >> 20) & 0xF, 0 | 1 | 2)),
            None => false,
        }
    }
}

/// Probe codec `cad` and map its widget graph.
///
/// Fails if the codec does not answer at all (no audio function group).
pub fn probe(s: &mut dyn VerbSender, cad: u32) -> Result<Codec, &'static str> {
    let vendor = s
        .verb(cad, 0, VERB_GET_PARAM, PARAM_VENDOR_ID)
        .map_err(|_| "codec not present")?;
    if vendor == 0 || vendor == 0xFFFF_FFFF {
        return Err("codec not present");
    }
    let subsystem = s
        .verb(cad, 0, VERB_GET_PARAM, PARAM_SUBSYSTEM_ID)
        .unwrap_or(0);
    let rev = s.verb(cad, 0, VERB_GET_PARAM, PARAM_REV_ID).unwrap_or(0);

    // Node 0 → function groups → pick the audio function group.
    let nc = s.verb(cad, 0, VERB_GET_PARAM, PARAM_NODE_COUNT)?;
    let fg_start = nc & 0xFF;
    let fg_count = (nc >> 16) & 0xFF;
    SerialPort::puts("[audio] codec ");
    SerialPort::put_u64(cad as u64);
    SerialPort::puts(" vendor=0x");
    SerialPort::put_hex(vendor as u64);
    SerialPort::puts(" root_node_count=0x");
    SerialPort::put_hex(nc as u64);
    SerialPort::puts("\n");
    let mut fg = None;
    for n in fg_start..fg_start + fg_count {
        let ft = s
            .verb(cad, n, VERB_GET_PARAM, PARAM_FUNCTION_TYPE)
            .unwrap_or(u32::MAX);
        SerialPort::puts("[audio] codec ");
        SerialPort::put_u64(cad as u64);
        SerialPort::puts(" fg nid=");
        SerialPort::put_u64(n as u64);
        SerialPort::puts(" type=0x");
        SerialPort::put_hex(ft as u64);
        SerialPort::puts("\n");
        if ft & 0xFF == GRP_AUDIO_FUNCTION {
            fg = Some(n);
            break;
        }
    }
    let fg = fg.ok_or("no audio function group")?;

    // Bring the codec out of its boot power state before probing.  Many real
    // codecs gate every verb on the function group being in D0 and return
    // nothing (or stall) while still powered down — which truncates the
    // widget walk and makes the codec look like it has no output path.  QEMU
    // never models power gating, so this is a no-op there.  Mirrors the Linux
    // HD-audio core, which powers the codec/AFG to D0 before its probe runs.
    let _ = s.verb(cad, fg, VERB_SET_POWER_STATE, PWR_D0);

    // Audio function group → its widgets.
    let wnc = s.verb(cad, fg, VERB_GET_PARAM, PARAM_NODE_COUNT)?;
    let wstart = wnc & 0xFF;
    let wcount = (wnc >> 16) & 0xFF;
    // Amplifier capabilities default to the audio function group's; widgets
    // that set `WCAP_AMP_OVRD` override them with their own.
    let afg_amp_in = s
        .verb(cad, fg, VERB_GET_PARAM, PARAM_AMP_IN_CAP)
        .unwrap_or(0);
    let afg_amp_out = s
        .verb(cad, fg, VERB_GET_PARAM, PARAM_AMP_OUT_CAP)
        .unwrap_or(0);
    let mut widgets = Vec::new();
    for n in wstart..wstart + wcount {
        let Ok(wcap) = s.verb(cad, n, VERB_GET_PARAM, PARAM_AUDIO_WIDGET_CAP) else {
            continue;
        };
        // Power each power-managed widget to D0 as it is discovered, so the
        // parameter reads below are answered (same gating issue as the FG).
        if wcap & WCAP_POWER != 0 {
            let _ = s.verb(cad, n, VERB_SET_POWER_STATE, PWR_D0);
        }
        let mut w = Widget {
            nid: n,
            wcap,
            pincap: 0,
            stream: 0,
            conns: [0; MAX_CONNS],
            nconns: 0,
            amp_in: afg_amp_in,
            amp_out: afg_amp_out,
            pin_cfg: 0,
        };
        match w.wtype() {
            WIDGET_PIN => {
                w.pincap = s.verb(cad, n, VERB_GET_PARAM, PARAM_PIN_CAP).unwrap_or(0);
                w.pin_cfg = read_pin_config(s, cad, n);
            }
            WIDGET_AUD_OUT | WIDGET_AUD_IN => {
                w.stream = s.verb(cad, n, VERB_GET_PARAM, PARAM_STREAM).unwrap_or(0);
            }
            _ => {}
        }
        if wcap & WCAP_AMP_OVRD != 0 {
            w.amp_in = s
                .verb(cad, n, VERB_GET_PARAM, PARAM_AMP_IN_CAP)
                .unwrap_or(w.amp_in);
            w.amp_out = s
                .verb(cad, n, VERB_GET_PARAM, PARAM_AMP_OUT_CAP)
                .unwrap_or(w.amp_out);
        }
        if wcap & WCAP_CONN_LIST != 0 {
            w.nconns = read_conns(s, cad, n, &mut w.conns);
        }
        widgets.push(w);
    }

    let (dac, out_pin, out_path) = find_output_path(&widgets);
    let (adc, in_pin, in_path) = find_input_path(&widgets);

    Ok(Codec {
        cad,
        vendor,
        subsystem,
        rev,
        fg,
        wnc,
        fmt: STREAM_FMT_48K_STEREO_16,
        dac,
        out_pin,
        out_path,
        adc,
        in_pin,
        in_path,
        widgets,
    })
}

/// Read the full connection list of `nid` into `out`.  Returns the count.
///
/// Short form: 4 one-byte entries per response; long form: 2 two-byte
/// entries.  Request indices advance by the number of entries per response.
///
/// Range entries (MSB set) are expanded: an entry whose top bit is set stands
/// for the run `[previous + 1 .. entry & mask]` (Linux `snd_hda_get_connections`
/// semantics).  This makes the stored list the *effective* connections, which
/// is also what `SET_CONNECT_SEL` indices refer to.
fn read_conns(s: &mut dyn VerbSender, cad: u32, nid: u32, out: &mut [u32; MAX_CONNS]) -> u32 {
    let len = s
        .verb(cad, nid, VERB_GET_PARAM, PARAM_CONNLIST_LEN)
        .unwrap_or(0);
    let count = (len & 0x7F) as usize;
    let long = len & 0x80 != 0;
    let step = if long { 2usize } else { 4usize };
    let mask = if long { 0xFFFFu32 } else { 0xFFu32 };
    let msb = if long { 0x8000u32 } else { 0x80u32 };
    let mut got = 0usize; // effective entries emitted (after range expansion)
    let mut slot = 0usize; // physical entries consumed
    let mut idx = 0usize;
    let mut prev = 0u32;
    while slot < count && got < MAX_CONNS && idx < count {
        let Ok(r) = s.verb(cad, nid, VERB_GET_CONNECT_LIST, idx as u32) else {
            break;
        };
        let mut shift = 0u32;
        while slot < count && got < MAX_CONNS && shift < 32 {
            let entry = (r >> shift) & mask;
            shift += if long { 16 } else { 8 };
            slot += 1;
            if entry == 0 {
                prev = 0;
                continue;
            }
            if entry & msb != 0 {
                // Range: [prev + 1 ..= entry & mask].  Empty/invalid ranges
                // (prev + 1 > end) contribute nothing.
                let last = entry & !msb;
                let mut v = prev + 1;
                while v <= last && got < MAX_CONNS {
                    out[got] = v;
                    got += 1;
                    v += 1;
                }
                // Ranges are the final entry per spec, but clamp `prev` to the
                // range endpoint anyway so a spec-violating follow-up entry is
                // still expanded from a consistent base instead of a stale
                // intermediate value when MAX_CONNS truncated the expansion.
                // An empty range (prev + 1 > last) leaves `prev` unchanged.
                if v > prev + 1 {
                    prev = last;
                }
            } else {
                out[got] = entry;
                got += 1;
                prev = entry;
            }
        }
        idx += step;
    }
    got as u32
}

/// Read a pin's default configuration (7.3.3.15).
///
/// `GET_CONFIG_DEFAULT` returns the full 32-bit configuration register in a
/// single response; the byte-per-index verbs are `SET_CONFIG_DEFAULT_BYTES_0..3`
/// (used for writing).  QEMU and real codecs alike answer index 0 with the
/// whole config, so one read suffices.
fn read_pin_config(s: &mut dyn VerbSender, cad: u32, nid: u32) -> u32 {
    s.verb(cad, nid, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(0)
}

/// Preference score for an output pin by its default-config device type:
/// analogue line-out / speaker / headphone first, digital next, inputs last.
fn pin_pref(dev: u32) -> u32 {
    match dev {
        0 | 1 | 2 => 0,       // line-out, speaker, hp-out
        4 | 5 => 1,           // spdif-out, dig-other-out
        8 | 9 | 10 | 11 => 2, // line-in, aux, mic-in, telephony
        _ => 3,
    }
}

/// Find an output path: a pin with `PINCAP_OUT`, then a reverse walk of the
/// connection graph to the first Audio Output converter feeding it.
fn find_output_path(widgets: &[Widget]) -> (Option<u32>, Option<u32>, Vec<(u32, usize)>) {
    // Candidate out pins, scored by default-config device type.  Stable
    // sort keeps the lowest nid first on ties.
    let mut scored: Vec<(u32, u32)> = widgets
        .iter()
        .filter(|w| w.wtype() == WIDGET_PIN && w.pincap & PINCAP_OUT != 0)
        .map(|w| (pin_pref((w.pin_cfg >> 20) & 0xF), w.nid))
        .collect();
    scored.sort_by_key(|(score, nid)| (*score, *nid));
    for (_, pin) in scored {
        if let Some((dac, path)) = reach_converter(widgets, pin, WIDGET_AUD_OUT) {
            return (Some(dac), Some(pin), path);
        }
    }
    (None, None, Vec::new())
}

/// Walk the connection graph from `start` toward its sources and return the
/// first widget of type `target` plus the path of widgets from it back to
/// `start` (inclusive on both ends), each hop tagged with the connection-list
/// index it uses toward the source.  Bounded depth; no hardcoded NIDs.
fn reach_converter(
    widgets: &[Widget],
    start: u32,
    target: u32,
) -> Option<(u32, Vec<(u32, usize)>)> {
    // parents[(nid)] = the widget whose connlist contains nid.
    let mut parents: Vec<(u32, u32)> = Vec::new();
    let mut frontier: Vec<u32> = alloc::vec![start];
    for _ in 0..8 {
        let mut next: Vec<u32> = Vec::new();
        for &nid in &frontier {
            let Some(w) = widgets.iter().find(|w| w.nid == nid) else {
                continue;
            };
            for &c in w.conns[..w.nconns as usize].iter() {
                if c == 0 {
                    continue;
                }
                if c == start || parents.iter().any(|(n, _)| *n == c) {
                    continue;
                }
                let Some(cw) = widgets.iter().find(|w| w.nid == c) else {
                    continue;
                };
                if cw.wtype() == target {
                    // Reconstruct start → … → c backwards.
                    let mut path = alloc::vec![c];
                    let mut cur = nid;
                    loop {
                        path.push(cur);
                        if cur == start {
                            break;
                        }
                        cur = parents
                            .iter()
                            .find(|(n, _)| *n == cur)
                            .map(|(_, p)| *p)
                            .unwrap_or(start);
                        if path.len() > MAX_CONNS {
                            return None;
                        }
                    }
                    return Some((c, path_hops(widgets, &path)));
                }
                if cw.wtype() == WIDGET_MIXER || cw.wtype() == WIDGET_SEL {
                    parents.push((c, nid));
                    next.push(c);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    None
}

/// Tag each node of a source-first path with the connection-list index it
/// uses toward the source (the position of the previous hop in its own
/// connection list).  The path head has no upstream neighbour.
fn path_hops(widgets: &[Widget], path: &[u32]) -> Vec<(u32, usize)> {
    let mut hops = Vec::with_capacity(path.len());
    for (j, &nid) in path.iter().enumerate() {
        let idx = if j == 0 {
            0
        } else if let Some(w) = widgets.iter().find(|w| w.nid == nid) {
            w.conns[..w.nconns as usize]
                .iter()
                .position(|&c| c == path[j - 1])
                .unwrap_or(0)
        } else {
            0
        };
        hops.push((nid, idx));
    }
    hops
}

/// Find an input path: the first Audio Input converter, then a walk upstream
/// (capture flows pin → … → ADC, so the ADC's connection list leads back to
/// the pin) to an in-capable pin.
fn find_input_path(widgets: &[Widget]) -> (Option<u32>, Option<u32>, Vec<(u32, usize)>) {
    for w in widgets {
        if w.wtype() != WIDGET_AUD_IN {
            continue;
        }
        if let Some((pin, path)) = reach_in_pin(widgets, w.nid) {
            return (Some(w.nid), Some(pin), path);
        }
    }
    (None, None, Vec::new())
}

/// From an ADC, follow its connection list upstream to the first in-capable
/// pin (possibly through mixers/selectors).  Returns the pin and the source-
/// first path `pin .. adc` tagged with per-hop connection indices.
fn reach_in_pin(widgets: &[Widget], adc: u32) -> Option<(u32, Vec<(u32, usize)>)> {
    let mut visited: Vec<u32> = alloc::vec![adc];
    let mut frontier: Vec<u32> = alloc::vec![adc];
    // parents[(source)] = the downstream node whose connlist contains it.
    let mut parents: Vec<(u32, u32)> = Vec::new();
    for _ in 0..8 {
        let mut next: Vec<u32> = Vec::new();
        for &nid in &frontier {
            let Some(w) = widgets.iter().find(|w| w.nid == nid) else {
                continue;
            };
            for &c in w.conns[..w.nconns as usize].iter() {
                if c == 0 || visited.iter().any(|v| *v == c) {
                    continue;
                }
                visited.push(c);
                let Some(cw) = widgets.iter().find(|w| w.nid == c) else {
                    continue;
                };
                if cw.wtype() == WIDGET_PIN {
                    if cw.pincap & PINCAP_IN != 0 {
                        // Reconstruct pin → … → adc (already source-first).
                        let mut path = alloc::vec![c];
                        let mut cur = nid;
                        loop {
                            path.push(cur);
                            if cur == adc {
                                break;
                            }
                            cur = parents
                                .iter()
                                .find(|(n, _)| *n == cur)
                                .map(|(_, p)| *p)
                                .unwrap_or(adc);
                            if path.len() > MAX_CONNS {
                                return None;
                            }
                        }
                        return Some((c, path_hops(widgets, &path)));
                    }
                } else if cw.wtype() == WIDGET_MIXER || cw.wtype() == WIDGET_SEL {
                    parents.push((c, nid));
                    next.push(c);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    None
}

fn set_power_d0(s: &mut dyn VerbSender, c: &Codec, nid: u32) {
    let _ = s.verb(c.cad, nid, VERB_SET_POWER_STATE, PWR_D0);
}

fn unmute_amp(
    s: &mut dyn VerbSender,
    c: &Codec,
    nid: u32,
    output: bool,
    wcap: u32,
    cap: u32,
    index: usize,
) {
    let supported = if output {
        wcap & WCAP_OUT_AMP
    } else {
        wcap & WCAP_IN_AMP
    };
    if supported == 0 {
        return;
    }
    let dir = if output {
        AMP_SET_OUTPUT
    } else {
        AMP_SET_INPUT
    };
    // Full, unmuted gain = the number of steps the (effective) amp capability
    // advertises — never a blind 0x7F.
    let gain = (cap & AMPCAP_NUM_STEPS) >> 8;
    let payload =
        dir | AMP_SET_LEFT | AMP_SET_RIGHT | (((index as u32) << 8) & AMP_INDEX_MASK) | gain;
    let _ = s.verb(c.cad, nid, VERB_SET_AMP_GAIN_MUTE, payload);
}

/// Highest VREF bias a pin supports, if any: 100% when the 80%/100% pin-cap
/// bit is set, else 50%, else ground.  `None` for pins without VREF circuitry.
fn pick_vref(pincap: u32) -> Option<u32> {
    if pincap & PINCAP_VREF_MASK == 0 {
        return None;
    }
    if pincap & (1 << 13) != 0 {
        Some(PIN_VREF_100)
    } else if pincap & (1 << 12) != 0 {
        Some(PIN_VREF_50)
    } else if pincap & (1 << 8) != 0 {
        Some(PIN_VREF_GRD)
    } else {
        None
    }
}

/// Bring up the output path: wake power domains, route selectors, unmute amps
/// at the connection index each hop uses, enable the pin (HP_EN for headphone
/// jacks, EAPD when present), then bind the converter to `tag`.
pub fn setup_output(s: &mut dyn VerbSender, c: &Codec, tag: u32) -> Result<(), &'static str> {
    let dac = c.dac.ok_or("codec has no DAC")?;
    set_power_d0(s, c, c.fg);
    for &(n, _) in c.out_path.iter() {
        if let Some(w) = c.widget(n) {
            if w.wcap & WCAP_POWER != 0 {
                set_power_d0(s, c, n);
            }
        }
    }
    // Route and unmute, converter → pin.  Each hop is unmuted at the
    // connection index it follows toward the source; selector inputs are
    // selected with the same index.  Converter/pin output amps use index 0.
    for &(n, idx) in c.out_path.iter() {
        let Some(w) = c.widget(n) else { continue };
        match w.wtype() {
            WIDGET_AUD_OUT => unmute_amp(s, c, n, true, w.wcap, w.amp_out, 0),
            WIDGET_SEL => {
                if w.nconns > 1 {
                    let _ = s.verb(c.cad, n, VERB_SET_CONNECT_SEL, idx as u32);
                }
                unmute_amp(s, c, n, false, w.wcap, w.amp_in, idx);
            }
            WIDGET_MIXER => unmute_amp(s, c, n, false, w.wcap, w.amp_in, idx),
            WIDGET_PIN => {
                unmute_amp(s, c, n, true, w.wcap, w.amp_out, 0);
                if w.nconns > 1 {
                    let _ = s.verb(c.cad, n, VERB_SET_CONNECT_SEL, idx as u32);
                }
            }
            _ => {}
        }
    }
    if let Some(pin) = c.out_pin {
        let mut pctl = PIN_OUT_EN;
        if let Some(w) = c.widget(pin) {
            if (w.pin_cfg >> 20) & 0xF == JACK_HP_OUT {
                pctl |= PIN_HP_EN;
            }
            if w.pincap & PINCAP_EAPD != 0 {
                // ALC269-family codecs gate EAPD on COEF 0x10 bit 9; clear it
                // first so the pin's EAPD is actually verb-controlled.
                realtek_eapd_verb_control(s, c);
                let _ = s.verb(c.cad, pin, VERB_SET_EAPD_BTLENABLE, EAPD_EN);
            }
        }
        let _ = s.verb(c.cad, pin, VERB_SET_PIN_WIDGET_CONTROL, pctl);
    }
    s.verb(c.cad, dac, VERB_SET_CONV, (tag << 4) | 0)?;
    s.verb(c.cad, dac, VERB_SET_STREAM_FORMAT, c.fmt as u32)?;
    Ok(())
}

/// Bring up the ALC256 analog output path with hardcoded nids, used when the
/// generic widget walk cannot enumerate the codec (see [`is_realtek_alc256`]):
/// power the function group, DAC 0x02 and the speaker/headphone pins 0x14/0x21
/// to D0, clear the EAPD verb-control COEF so pin EAPD is honoured, unmute the
/// DAC and both pin output amps at full gain, enable the speaker pin
/// (0x707 = 0x40) and its EAPD (0x70C = 0x02) plus the headphone pin
/// (0x707 = 0xC0), then bind the DAC to `tag`.
pub fn setup_alc256_output(
    s: &mut dyn VerbSender,
    c: &Codec,
    tag: u32,
) -> Result<(), &'static str> {
    let _ = s.verb(c.cad, c.fg, VERB_SET_POWER_STATE, PWR_D0);
    for nid in [RTL_ALC256_DAC, RTL_ALC256_SPKR_PIN, RTL_ALC256_HP_PIN] {
        let _ = s.verb(c.cad, nid, VERB_SET_POWER_STATE, PWR_D0);
    }
    realtek_eapd_verb_control(s, c);
    // Full-gain, unmuted output amp (index 0) on the converter and both pins.
    let amp = AMP_SET_OUTPUT | AMP_SET_LEFT | AMP_SET_RIGHT | 0x7F;
    let _ = s.verb(c.cad, RTL_ALC256_DAC, VERB_SET_AMP_GAIN_MUTE, amp);
    let _ = s.verb(
        c.cad,
        RTL_ALC256_SPKR_PIN,
        VERB_SET_PIN_WIDGET_CONTROL,
        PIN_OUT_EN,
    );
    let _ = s.verb(c.cad, RTL_ALC256_SPKR_PIN, VERB_SET_EAPD_BTLENABLE, EAPD_EN);
    let _ = s.verb(c.cad, RTL_ALC256_SPKR_PIN, VERB_SET_AMP_GAIN_MUTE, amp);
    let _ = s.verb(
        c.cad,
        RTL_ALC256_HP_PIN,
        VERB_SET_PIN_WIDGET_CONTROL,
        PIN_OUT_EN | PIN_HP_EN,
    );
    let _ = s.verb(c.cad, RTL_ALC256_HP_PIN, VERB_SET_AMP_GAIN_MUTE, amp);
    s.verb(c.cad, RTL_ALC256_DAC, VERB_SET_CONV, (tag << 4) | 0)?;
    s.verb(c.cad, RTL_ALC256_DAC, VERB_SET_STREAM_FORMAT, c.fmt as u32)?;
    Ok(())
}

/// Bring up the input path (probing: configure the codec side only — capture
/// is not exposed by the audio engine yet).  Routes selectors, unmutes amps at
/// the per-hop connection index, and biases the in-pin with VREF where its pin
/// capability advertises it.
pub fn setup_input(s: &mut dyn VerbSender, c: &Codec, tag: u32) -> Result<(), &'static str> {
    let adc = c.adc.ok_or("codec has no ADC")?;
    set_power_d0(s, c, c.fg);
    for &(n, _) in c.in_path.iter() {
        if let Some(w) = c.widget(n) {
            if w.wcap & WCAP_POWER != 0 {
                set_power_d0(s, c, n);
            }
        }
    }
    // Route and unmute, pin → converter.  The ADC's input amp is unmuted at
    // the connection index it follows; selector inputs are selected likewise.
    for &(n, idx) in c.in_path.iter() {
        let Some(w) = c.widget(n) else { continue };
        match w.wtype() {
            WIDGET_AUD_IN => unmute_amp(s, c, n, false, w.wcap, w.amp_in, idx),
            WIDGET_SEL => {
                if w.nconns > 1 {
                    let _ = s.verb(c.cad, n, VERB_SET_CONNECT_SEL, idx as u32);
                }
                unmute_amp(s, c, n, false, w.wcap, w.amp_in, idx);
            }
            WIDGET_MIXER => unmute_amp(s, c, n, false, w.wcap, w.amp_in, idx),
            _ => {}
        }
    }
    if let Some(pin) = c.in_pin {
        let mut pctl = PIN_IN_EN;
        if let Some(w) = c.widget(pin) {
            if let Some(vref) = pick_vref(w.pincap) {
                pctl |= vref;
            }
        }
        let _ = s.verb(c.cad, pin, VERB_SET_PIN_WIDGET_CONTROL, pctl);
    }
    s.verb(c.cad, adc, VERB_SET_CONV, (tag << 4) | 0)?;
    s.verb(c.cad, adc, VERB_SET_STREAM_FORMAT, c.fmt as u32)?;
    Ok(())
}
