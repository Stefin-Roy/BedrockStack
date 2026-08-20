use alloc::vec;
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;

use super::boot::BootSector;
use super::mount::NtfsSuperBlock;
use super::runs::{RunList, read_file_at};

pub const RECORD_MAGIC: u32 = 0x454C_4946; // "FILE"

#[inline]
fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    let s = d.get(o..o + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    let s = d.get(o..o + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Apply the update sequence array fixup: every 512-byte sector tail inside
/// the record must carry the USN; the saved sector tails are restored from
/// the USA.  A mismatch (torn write or not an MFT/INDX structure) is an
/// error, never a panic.
pub(crate) fn usa_fixup(data: &mut [u8], sector_size: usize) -> Result<(), VfsError> {
    let usa_off = u16_at(data, 0x04).ok_or(VfsError::IOError)? as usize;
    let usa_count = u16_at(data, 0x06).ok_or(VfsError::IOError)? as usize;
    if usa_off < 0x08 || usa_count < 1 {
        return Err(VfsError::IOError);
    }
    if data.len() % sector_size != 0 || data.len() < sector_size {
        return Err(VfsError::IOError);
    }
    if usa_off + usa_count * 2 > data.len() {
        return Err(VfsError::IOError);
    }

    let usn = u16_at(data, usa_off).ok_or(VfsError::IOError)?;
    if u16_at(data, sector_size - 2).ok_or(VfsError::IOError)? != usn {
        return Err(VfsError::IOError);
    }

    let mut idx = usa_off + 2;
    let mut sector = 1usize;
    for _ in 1..usa_count {
        // The USN overwrites the *last* two bytes of every sector; the
        // original tails are saved in the USA array and restored here.
        let tail = sector * sector_size + sector_size - 2;
        if tail + 2 > data.len() {
            return Err(VfsError::IOError);
        }
        if u16_at(data, tail).ok_or(VfsError::IOError)? != usn {
            return Err(VfsError::IOError);
        }
        let saved = u16_at(data, idx).ok_or(VfsError::IOError)?;
        data[tail] = (saved & 0xFF) as u8;
        data[tail + 1] = (saved >> 8) as u8;
        idx += 2;
        sector += 1;
    }
    Ok(())
}

/// Read an MFT record through a run list (the $MFT's own runs), applying the
/// USA fixup and checking the FILE magic.  `offset` is a byte offset into the
/// file described by `runs` (i.e. mft_no * record_size for the MFT).
pub(crate) fn read_record_at(
    device: &dyn BlockDevice,
    boot: &BootSector,
    runs: &RunList,
    offset: u64,
    record_size: u64,
) -> Result<Vec<u8>, VfsError> {
    let mut buf = vec![0u8; record_size as usize];
    let n = read_file_at(device, boot, runs, offset, &mut buf)?;
    if n < record_size as usize {
        return Err(VfsError::IOError);
    }
    usa_fixup(&mut buf, boot.bytes_per_sector as usize)?;
    if u32_at(&buf, 0).ok_or(VfsError::IOError)? != RECORD_MAGIC {
        return Err(VfsError::IOError);
    }
    Ok(buf)
}

/// Read MFT record `mft_no` of the volume.
pub(crate) fn read_mft_record(sb: &NtfsSuperBlock, mft_no: u64) -> Result<Vec<u8>, VfsError> {
    if mft_no >= sb.boot.max_records {
        return Err(VfsError::NotFound);
    }
    let offset = mft_no
        .checked_mul(sb.boot.record_size)
        .ok_or(VfsError::IOError)?;
    read_record_at(
        &*sb.device,
        &sb.boot,
        &sb.mft_runs,
        offset,
        sb.boot.record_size,
    )
}
