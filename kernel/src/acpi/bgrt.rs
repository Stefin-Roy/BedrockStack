//! BGRT (Boot Graphics Resource Table) parser and BMP blitter.
//!
//! Parses the ACPI BGRT table (signature `"BGRT"`), validates it, locates the
//! firmware boot image physical address, maps it, validates the BMP file at
//! that address, and blits it onto the framebuffer. All firmware data is
//! treated as untrusted — every length / offset / dimension is bounds-checked
//! and overflow-checked. Failures fall back to the hex logo.

use crate::drivers::serial::SerialPort;
use framebuffer::{Color, Display, Framebuffer};

use super::platform::AcpiError;
use super::tables::SdtEntry;

// ── BGRT table ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct BgrtInfo {
    pub version: u16,
    pub status: u8,
    pub image_type: u8,
    pub image_address: u64,
    pub offset_x: u32,
    pub offset_y: u32,
}

fn read_u16_le(p: &[u8], off: usize) -> Option<u16> {
    let b = p.get(off..off + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}
fn read_u32_le(p: &[u8], off: usize) -> Option<u32> {
    let b = p.get(off..off + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn read_u64_le(p: &[u8], off: usize) -> Option<u64> {
    let b = p.get(off..off + 8)?;
    Some(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// Parse a mapped BGRT entry. `vaddr` must point to a validated ACPI SDT
/// (checksum already checked by `tables::map_sdt`) with length `len`.
pub fn parse_bgrt(entry: &SdtEntry) -> Result<BgrtInfo, AcpiError> {
    // BGRT version 1 is 56 bytes (0x38) including 36-byte header.
    if entry.length < 56 {
        log::warn!("BGRT: table too short (len={})", entry.length);
        SerialPort::puts("[bgrt] too short len=");
        SerialPort::put_u64(entry.length as u64);
        SerialPort::puts("\n");
        return Err(AcpiError::InvalidData);
    }
    let raw = unsafe { core::slice::from_raw_parts(entry.vaddr as *const u8, entry.length as usize) };
    // Validate signature already done by caller; double-check
    if raw.get(0..4) != Some(b"BGRT") {
        return Err(AcpiError::BadSignature);
    }
    // Checksum already validated, but we tolerate reserved bits etc.
    let version = read_u16_le(raw, 36).ok_or(AcpiError::InvalidData)?;
    let status = *raw.get(38).ok_or(AcpiError::InvalidData)?;
    let image_type = *raw.get(39).ok_or(AcpiError::InvalidData)?;
    let image_address = read_u64_le(raw, 40).ok_or(AcpiError::InvalidData)?;
    let offset_x = read_u32_le(raw, 48).ok_or(AcpiError::InvalidData)?;
    let offset_y = read_u32_le(raw, 52).ok_or(AcpiError::InvalidData)?;

    log::info!(
        "BGRT: version={} status=0x{:02x} type={} addr=0x{:x} offset=({},{})",
        version,
        status,
        image_type,
        image_address,
        offset_x,
        offset_y
    );
    SerialPort::puts("[bgrt] version=");
    SerialPort::put_u64(version as u64);
    SerialPort::puts(" status=0x");
    SerialPort::put_hex(status as u64);
    SerialPort::puts(" type=");
    SerialPort::put_u64(image_type as u64);
    SerialPort::puts(" addr=0x");
    SerialPort::put_hex(image_address);
    SerialPort::puts(" off=");
    SerialPort::put_u64(offset_x as u64);
    SerialPort::puts(",");
    SerialPort::put_u64(offset_y as u64);
    SerialPort::puts("\n");

    if version != 1 {
        log::warn!("BGRT: unsupported version {}", version);
        SerialPort::puts("[bgrt] unsupported version\n");
        return Err(AcpiError::InvalidData);
    }
    if image_type != 0 {
        log::warn!("BGRT: unsupported image type {}", image_type);
        SerialPort::puts("[bgrt] unsupported image type\n");
        return Err(AcpiError::InvalidData);
    }
    // Displayed bit must be set. Reserved bits ignored.
    if status & 0x01 == 0 {
        log::warn!("BGRT: image not displayed (status=0x{:02x})", status);
        SerialPort::puts("[bgrt] not displayed\n");
        return Err(AcpiError::TableNotFound);
    }
    if image_address == 0 {
        log::warn!("BGRT: image_address is 0");
        SerialPort::puts("[bgrt] zero address\n");
        return Err(AcpiError::InvalidData);
    }

    Ok(BgrtInfo {
        version,
        status,
        image_type,
        image_address,
        offset_x,
        offset_y,
    })
}

// ── BMP parsing ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct BmpInfo {
    width: u32,
    height: u32, // absolute
    top_down: bool,
    bpp: u16,
    row_stride: usize,
    bf_off_bits: usize,
}

fn parse_bmp(slice: &[u8]) -> Result<BmpInfo, &'static str> {
    if slice.len() < 14 {
        return Err("BMP too small for file header");
    }
    let bf_type = u16::from_le_bytes([slice[0], slice[1]]);
    if bf_type != 0x4D42 {
        return Err("BMP bad magic");
    }
    let bf_size = u32::from_le_bytes([slice[2], slice[3], slice[4], slice[5]]) as usize;
    let bf_off_bits = u32::from_le_bytes([slice[10], slice[11], slice[12], slice[13]]) as usize;

    if bf_size < 14 || bf_size > slice.len() {
        // bfSize may be larger than mapped slice if we only mapped header window.
        // Caller ensures slice is at least bfSize bytes if mapping succeeded.
        // For early header-only check, allow bfSize > slice.len() only if caller will remap.
        // Here we require at least header sizes; deeper validation done after full map.
        if bf_size > 32 * 1024 * 1024 {
            return Err("BMP bfSize unreasonably large");
        }
        // If slice is header-only window, defer size check
        if bf_size > slice.len() && slice.len() < 64 {
            // header-only probe: can't fully validate yet
        } else if bf_size != slice.len() && bf_size > slice.len() {
            return Err("BMP bfSize exceeds mapped slice");
        }
    }
    if bf_off_bits < 14 || bf_off_bits >= slice.len() && slice.len() >= 64 {
        // defer if header-only
        if slice.len() >= 54 {
            return Err("BMP bfOffBits out of range");
        }
    }

    if slice.len() < 14 + 40 {
        return Err("BMP too small for info header");
    }
    let bi_size = u32::from_le_bytes([slice[14], slice[15], slice[16], slice[17]]) as usize;
    if bi_size < 40 {
        return Err("BMP biSize <40");
    }
    if slice.len() < 14 + bi_size {
        return Err("BMP truncated info header");
    }
    let bi_width = i32::from_le_bytes([slice[18], slice[19], slice[20], slice[21]]);
    let bi_height = i32::from_le_bytes([slice[22], slice[23], slice[24], slice[25]]);
    let bi_planes = u16::from_le_bytes([slice[26], slice[27]]);
    let bi_bit_count = u16::from_le_bytes([slice[28], slice[29]]);
    let bi_compression = u32::from_le_bytes([slice[30], slice[31], slice[32], slice[33]]);
    //let bi_size_image = u32::from_le_bytes([slice[34], slice[35], slice[36], slice[37]]);

    if bi_planes != 1 {
        return Err("BMP biPlanes !=1");
    }
    if bi_bit_count != 24 && bi_bit_count != 32 {
        return Err("BMP unsupported bitcount");
    }
    if bi_compression != 0 {
        return Err("BMP compression not BI_RGB");
    }
    if bi_width <= 0 {
        return Err("BMP width <=0");
    }
    if bi_height == 0 {
        return Err("BMP height ==0");
    }
    let width = bi_width as u32;
    let abs_height = bi_height.unsigned_abs();
    let top_down = bi_height < 0;

    // Overflow-checked row stride
    let bytes_pp = (bi_bit_count as usize) / 8;
    let row_bytes = (width as usize)
        .checked_mul(bytes_pp)
        .ok_or("BMP row_bytes overflow")?;
    let row_stride = if bi_bit_count == 24 {
        // pad to 4 bytes
        row_bytes
            .checked_add(3)
            .ok_or("BMP row stride overflow")?
            & !3usize
    } else {
        row_bytes
    };

    let image_size = row_stride
        .checked_mul(abs_height as usize)
        .ok_or("BMP image size overflow")?;

    // bfOffBits + image_size must fit in bfSize (and slice)
    // For header-only probe, image_size may exceed but defer
    if slice.len() >= 54 {
        if bf_off_bits.checked_add(image_size).is_none() {
            return Err("BMP offset+image overflow");
        }
        let off_end = bf_off_bits + image_size;
        // If we have full file mapped, off_end must <= bfSize and <= slice.len()
        if slice.len() >= bf_size && bf_size != 0 {
            if off_end > bf_size {
                return Err("BMP pixel data exceeds bfSize");
            }
        }
        if off_end > slice.len() && slice.len() > 64 {
            return Err("BMP pixel data exceeds slice");
        }
    }

    // sanity: limit dimensions to avoid huge blit (e.g., 8k)
    if width > 4096 || abs_height > 4096 {
        return Err("BMP dimensions too large");
    }

    Ok(BmpInfo {
        width,
        height: abs_height,
        top_down,
        bpp: bi_bit_count,
        row_stride,
        bf_off_bits,
    })
}

// ── physical mapping helper ────────────────────────────────────────────

fn map_physical_slice(paddr: u64, len: usize) -> Option<&'static [u8]> {
    if paddr == 0 || len == 0 {
        return None;
    }
    if len > 32 * 1024 * 1024 {
        SerialPort::puts("[bgrt] map len too large\n");
        return None;
    }
    // overflow check
    let _end = paddr.checked_add(len as u64)?;
    let offset = paddr & 0xFFF;
    let aligned = paddr - offset;
    let total = (len as u64).checked_add(offset)?;
    let pages = (total + 0xFFF) & !0xFFF;
    let vaddr = crate::acpi::try_map_device_mmio(
        aligned,
        pages,
        crate::mm::vmm::PageFlags::READ,
    )
    .ok()?;
    let start = (vaddr + offset) as *const u8;
    Some(unsafe { core::slice::from_raw_parts(start, len) })
}

// ── public blitter ─────────────────────────────────────────────────────

/// Try to blit the BGRT BMP onto `fb` centred at (`cx`,`cy`).
///
/// Returns `Some((width,height))` on success so the caller can space the
/// surrounding text, or `None` if BGRT is absent/invalid and the fallback
/// (hex) should be used.
pub fn blit_bgrt_logo(fb: &mut Framebuffer, cx: usize, cy: usize) -> Option<(usize, usize)> {
    let info = crate::acpi::global_snapshot()
        .and_then(|s| s.bgrt.clone())
        .or_else(|| {
            SerialPort::puts("[bgrt] no global BGRT snapshot\n");
            None
        })?;

    SerialPort::puts("[bgrt] trying blit addr=0x");
    SerialPort::put_hex(info.image_address);
    SerialPort::puts("\n");

    // Map a small header window first to learn bfSize without mapping megabytes speculatively.
    let hdr_slice = map_physical_slice(info.image_address, 64)?;
    // Peek bfSize if it looks like BMP, else we need to guess size.
    let bf_size_guess = if hdr_slice.len() >= 6 && hdr_slice[0] == b'B' && hdr_slice[1] == b'M' {
        let sz = u32::from_le_bytes([hdr_slice[2], hdr_slice[3], hdr_slice[4], hdr_slice[5]]) as usize;
        if sz < 54 || sz > 16 * 1024 * 1024 {
            SerialPort::puts("[bgrt] bogus bfSize\n");
            return None;
        }
        sz
    } else {
        // Not a BMP file? Per doc we could try raw bitmap path, but Linux assumes BMP.
        // Fall back to header-only parse will reject and we'll fallback to hex.
        SerialPort::puts("[bgrt] not BMP magic\n");
        return None;
    };

    // Now map the whole file.
    let bmp_slice = map_physical_slice(info.image_address, bf_size_guess)?;
    let bmp = match parse_bmp(bmp_slice) {
        Ok(b) => b,
        Err(e) => {
            SerialPort::puts("[bgrt] BMP parse err: ");
            SerialPort::puts(e);
            SerialPort::puts("\n");
            log::warn!("BGRT BMP parse failed: {}", e);
            return None;
        }
    };

    log::info!(
        "BGRT BMP {}x{} bpp={} stride={} off={}",
        bmp.width,
        bmp.height,
        bmp.bpp,
        bmp.row_stride,
        bmp.bf_off_bits
    );
    SerialPort::puts("[bgrt] BMP ");
    SerialPort::put_u64(bmp.width as u64);
    SerialPort::puts("x");
    SerialPort::put_u64(bmp.height as u64);
    SerialPort::puts(" bpp=");
    SerialPort::put_u64(bmp.bpp as u64);
    SerialPort::puts("\n");

    // Clip dimensions to framebuffer to avoid enormous blit, but keep aspect.
    // If image is wider than framebuffer, we still centre and clip per-pixel.
    let img_w = bmp.width as usize;
    let img_h = bmp.height as usize;
    let fb_w = fb.width();
    let fb_h = fb.height();
    if img_w == 0 || img_h == 0 || fb_w == 0 || fb_h == 0 {
        return None;
    }

    // Top-left dest so image is centred at (cx,cy)
    let dst_x0 = if img_w / 2 <= cx {
        cx - img_w / 2
    } else {
        0
    };
    let dst_y0 = if img_h / 2 <= cy {
        cy - img_h / 2
    } else {
        0
    };

    let bytes_pp = (bmp.bpp as usize) / 8;
    let pixels_base = bmp.bf_off_bits;

    // Sanity: pixels must be within slice
    if pixels_base + bmp.row_stride * img_h > bmp_slice.len() {
        SerialPort::puts("[bgrt] pixel data out of range\n");
        return None;
    }

    // Blit row by row. For 24bpp: pixel bytes are B,G,R. For 32bpp: B,G,R,A(reserved).
    // Use put_pixel which handles pixel_format conversion.
    for img_y in 0..img_h {
        let dst_y = dst_y0 + img_y;
        if dst_y >= fb_h {
            break;
        }
        // File row index
        let file_row = if bmp.top_down {
            img_y
        } else {
            img_h - 1 - img_y
        };
        let row_start = pixels_base + file_row * bmp.row_stride;
        if row_start + img_w * bytes_pp > bmp_slice.len() {
            break;
        }
        for img_x in 0..img_w {
            let dst_x = dst_x0 + img_x;
            if dst_x >= fb_w {
                break;
            }
            let off = row_start + img_x * bytes_pp;
            let (r, g, b) = if bmp.bpp == 24 {
                let b = bmp_slice[off];
                let g = bmp_slice[off + 1];
                let r = bmp_slice[off + 2];
                (r, g, b)
            } else {
                let b = bmp_slice[off];
                let g = bmp_slice[off + 1];
                let r = bmp_slice[off + 2];
                // let _a = bmp_slice[off+3]; // reserved
                (r, g, b)
            };
            // Optimization: skip magenta key? No, blit opaque. But skip pure black
            // background? OVMF BMP has white logo on black? Could be with alpha?
            // We blit all pixels opaque; BMP background will cover BG. That's fine
            // as logo is centred.
            let col = Color::new(r, g, b, 255);
            let _ = fb.put_pixel(dst_x, dst_y, col);
        }
    }

    // Also optionally use BGRT offset X/Y for debugging, but we centre instead.
    SerialPort::puts("[bgrt] blit ok\n");
    log::info!("BGRT blit {}x{} at ({},{})", img_w, img_h, dst_x0, dst_y0);
    Some((img_w, img_h))
}
