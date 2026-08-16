//! Minimal HID report-descriptor parser for the non-boot (generic protocol)
//! fallback path of the HID class driver.
//!
//! Full HID 1.11 descriptor parsing is out of scope; this walker extracts
//! exactly the fields the driver needs to bind a generic keyboard or mouse:
//!
//! - the **kind** from the top-level Application collection's usage
//!   (Generic Desktop: Keyboard = 1.6, Mouse/Pointer = 1.2/1.1),
//! - the **report byte length** by summing `Report Size × Report Count` over
//!   every `Input` main item (output/feature items are not part of the input
//!   report), rounded up to a byte boundary, plus one byte when a `Report
//!   ID` global item is present,
//! - the **output report byte length** from the `Output` main items (the
//!   keyboard LED report), computed the same way.
//!
//! Only short items are handled (long items `0xFE` are skipped).  Items are
//! the HID short-item prefix byte: bits[1:0] = data size (0/1/2/4 bytes),
//! bits[3:2] = item type (Main/Global/Local), bits[7:4] = tag.

use crate::usb::class::hid::HidKind;

/// Report layout information recovered from a report descriptor.
pub struct HidReportInfo {
    /// Input report byte length (rounded up, +1 if a Report ID is present).
    pub report_len: usize,
    /// Output report byte length (e.g. the keyboard LED report); 0 when the
    /// descriptor has no Output items.
    pub output_len: usize,
    /// The device kind decoded from the application collection usage.
    pub kind: HidKind,
}

/// The parsed report is interpreted boot-style by the driver even though the
/// device runs the generic protocol (modifier + 6 key usages for keyboards;
/// buttons / deltas for mice).  This covers boot-compatible layouts and
/// QEMU's `usb-kbd`/`usb-mouse`; absolute pointers (QEMU `usb-tablet`) are
/// diffed against the previous report so deltas are recovered.
pub fn parse_report_descriptor(desc: &[u8]) -> Option<HidReportInfo> {
    let mut usage_page: u16 = 0;
    let mut local_usage: u32 = 0;
    let mut report_size: u32 = 8; // HID defaults
    let mut report_count: u32 = 1;
    let mut has_report_id = false;
    let mut input_bits: u64 = 0;
    let mut output_bits: u64 = 0;
    let mut kind: Option<HidKind> = None;

    let mut off = 0;
    while off < desc.len() {
        let prefix = desc[off];
        off += 1;
        if prefix == 0xFE {
            // Long item: skip the length byte and payload.
            if off >= desc.len() {
                break;
            }
            let len = desc[off] as usize;
            off = (off + 1 + len).min(desc.len());
            continue;
        }
        let size = match prefix & 0x03 {
            0 => 0usize,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if off + size > desc.len() {
            break;
        }
        let data = &desc[off..off + size];
        off += size;
        let val = le_u32(data);

        match (prefix >> 2) & 0x03 {
            0 => {
                // Main items.
                match (prefix >> 4) & 0x0F {
                    8 => {
                        // Input: contributes to the input report.
                        input_bits += (report_size as u64) * (report_count as u64);
                        local_usage = 0;
                    }
                    9 => {
                        // Output: contributes to the output report (e.g. the
                        // keyboard LED report).
                        output_bits += (report_size as u64) * (report_count as u64);
                        local_usage = 0;
                    }
                    0xB => {
                        // Feature: part of neither report.
                        local_usage = 0;
                    }
                    0xA => {
                        // Collection: a top-level application collection names
                        // the device kind via its local usage on page 1.
                        if usage_page == 1 {
                            let usage = if local_usage != 0 {
                                local_usage
                            } else {
                                val & 0xFF
                            };
                            match usage {
                                0x06 => kind = Some(HidKind::Keyboard),
                                0x01 | 0x02 => kind = Some(HidKind::Mouse),
                                _ => {}
                            }
                        }
                        local_usage = 0;
                    }
                    _ => {}
                }
            }
            1 => {
                // Global items.
                match (prefix >> 4) & 0x0F {
                    0 => usage_page = val as u16,
                    7 => report_size = val,
                    8 => has_report_id = true,
                    9 => report_count = val,
                    _ => {}
                }
            }
            2 => {
                // Local items: Usage (0x08/0x09/0x0A).
                if (prefix >> 4) & 0x0F == 0 {
                    local_usage = val;
                }
            }
            _ => {}
        }
    }

    let kind = kind?;
    let bytes = ((input_bits + 7) / 8) as usize;
    let report_len = if has_report_id { bytes + 1 } else { bytes };
    if report_len == 0 || report_len > 4096 {
        return None;
    }
    let out_bytes = ((output_bits + 7) / 8) as usize;
    let output_len = if has_report_id {
        out_bytes + 1
    } else {
        out_bytes
    };
    if output_len > 4096 {
        return None;
    }
    Some(HidReportInfo {
        report_len,
        output_len,
        kind,
    })
}

fn le_u32(data: &[u8]) -> u32 {
    let mut v: u32 = 0;
    for (i, &b) in data.iter().enumerate() {
        v |= (b as u32) << (8 * i);
    }
    v
}
