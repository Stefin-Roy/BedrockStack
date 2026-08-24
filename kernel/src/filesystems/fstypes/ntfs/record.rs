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
    let usa_off = match u16_at(data, 0x04) {
        Some(v) => v as usize,
        None => {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: usa_off OOB");
            return Err(VfsError::IOError);
        }
    };
    let usa_count = match u16_at(data, 0x06) {
        Some(v) => v as usize,
        None => {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: usa_count OOB");
            return Err(VfsError::IOError);
        }
    };
    if usa_off < 0x08 || usa_count < 1 {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: bad usa_off/count");
        return Err(VfsError::IOError);
    }
    if data.len() % sector_size != 0 || data.len() < sector_size {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: record size not multiple of sector");
        return Err(VfsError::IOError);
    }
    if usa_off + usa_count * 2 > data.len() {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: USA array OOB");
        return Err(VfsError::IOError);
    }

    let usn = match u16_at(data, usa_off) {
        Some(v) => v,
        None => {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: usn OOB");
            return Err(VfsError::IOError);
        }
    };
    let tail0 = match u16_at(data, sector_size - 2) {
        Some(v) => v,
        None => {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: sector0 tail OOB");
            return Err(VfsError::IOError);
        }
    };
    if tail0 != usn {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: sector0 USN mismatch (torn write)");
        return Err(VfsError::IOError);
    }

    let sectors = data.len() / sector_size;
    // Spec: usa_count == sectors+1 (USN + one saved tail per sector).
    // Be strict so a malformed record is not silently accepted; the
    // diagnostic distinguishes this from a torn-write USN mismatch.
    if usa_count != sectors + 1 {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: usa_count != sectors+1");
        return Err(VfsError::IOError);
    }

    let mut idx = usa_off + 2;
    // Sector 0 already validated above; remaining sectors are 1..sectors
    for sector in 1..sectors {
        // Tail of sector `sector` is at (sector+1)*sector_size -2.
        let tail = sector * sector_size + sector_size - 2;
        if tail + 2 > data.len() {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: sector tail OOB");
            return Err(VfsError::IOError);
        }
        let tail_val = match u16_at(data, tail) {
            Some(v) => v,
            None => {
                crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: tail read OOB");
                return Err(VfsError::IOError);
            }
        };
        if tail_val != usn {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: sector USN mismatch (torn write)");
            return Err(VfsError::IOError);
        }
        let saved = match u16_at(data, idx) {
            Some(v) => v,
            None => {
                crate::filesystems::fstypes::ntfs::set_last_error_if_none("usa_fixup: saved tail OOB");
                return Err(VfsError::IOError);
            }
        };
        data[tail] = (saved & 0xFF) as u8;
        data[tail + 1] = (saved >> 8) as u8;
        idx += 2;
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
    let n = read_file_at(device, boot, runs, offset, &mut buf).map_err(|e| {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("read_record_at: read_file_at failed");
        e
    })?;
    if n < record_size as usize {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("read_record_at: short read");
        return Err(VfsError::IOError);
    }
    usa_fixup(&mut buf, boot.bytes_per_sector as usize).map_err(|e| {
        // usa_fixup already set detail; preserve.
        e
    })?;
    let magic = match u32_at(&buf, 0) {
        Some(v) => v,
        None => {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("read_record_at: magic OOB");
            return Err(VfsError::IOError);
        }
    };
    if magic != RECORD_MAGIC {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("read_record_at: FILE magic miss");
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
