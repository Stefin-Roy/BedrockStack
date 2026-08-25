use alloc::vec;
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::vfs::error::VfsError;

use super::boot::BootSector;
use super::io::read_sectors;

/// Cap a single device read at 252 KiB so it always fits the AHCI PRDT
/// (64 entries x 4 KiB) even mid-page — mirrors the FAT32 driver's limit.
const MAX_READ_BYTES: usize = 252 * 1024;

const MAX_RUNS: usize = 1_000_000;

/// One extent of a non-resident attribute.  `vcn`/`len` are in clusters;
/// a negative `lcn` marks a sparse hole (reads return zeros).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    pub vcn: u64,
    pub lcn: i64,
    pub len: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RunList {
    pub runs: Vec<Run>,
}

impl RunList {
    /// Find the run containing the byte offset (runs must be VCN-sorted).
    pub fn find(&self, offset: u64, cluster_size: u64) -> Option<&Run> {
        let vcn = offset / cluster_size;
        let idx = self.runs.partition_point(|r| r.vcn <= vcn);
        if idx == 0 {
            return None;
        }
        let r = &self.runs[idx - 1];
        if vcn < r.vcn.saturating_add(r.len) {
            Some(r)
        } else {
            None
        }
    }
}

/// Decode an attribute's mapping pairs into a run list.  Pair offsets are
/// deltas except the first (absolute); `base_vcn` shifts the decoded VCNs so
/// extents located by the $ATTRIBUTE_LIST land at their absolute position.
pub(crate) fn decode_mapping_pairs(data: &[u8], base_vcn: u64) -> Result<RunList, VfsError> {
    let mut runs = Vec::new();
    let mut vcn: i64 = 0;
    let mut lcn: i64 = 0;
    let mut pos = 0usize;

    loop {
        if runs.len() >= MAX_RUNS {
            return Err(VfsError::IOError);
        }
        let Some(&hdr) = data.get(pos) else {
            return Err(VfsError::IOError);
        };
        pos += 1;
        if hdr == 0 {
            break;
        }
        let len_len = (hdr & 0x0F) as usize;
        let off_len = (hdr >> 4) as usize;
        if len_len == 0 || len_len > 8 || off_len > 8 {
            return Err(VfsError::IOError);
        }
        if pos + len_len + off_len > data.len() {
            return Err(VfsError::IOError);
        }

        let mut length: u64 = 0;
        for i in 0..len_len {
            length |= (data[pos + i] as u64) << (8 * i);
        }
        if length == 0 || length > i64::MAX as u64 {
            return Err(VfsError::IOError);
        }

        if off_len == 0 {
            // Sparse run: advances the VCN, not the LCN.
            vcn = vcn.checked_add(length as i64).ok_or(VfsError::IOError)?;
            let abs = base_vcn.checked_add(vcn as u64).ok_or(VfsError::IOError)?;
            runs.push(Run {
                vcn: abs,
                lcn: -1,
                len: length,
            });
        } else {
            let mut raw: u64 = 0;
            for i in 0..off_len {
                raw |= (data[pos + len_len + i] as u64) << (8 * i);
            }
            // Sign-extend from bit 8*off_len (arithmetic shift).
            let shift = 64 - 8 * off_len;
            let delta = ((raw << shift) as i64) >> shift;
            lcn = lcn.checked_add(delta).ok_or(VfsError::IOError)?;
            vcn = vcn.checked_add(length as i64).ok_or(VfsError::IOError)?;
            let abs = base_vcn.checked_add(vcn as u64).ok_or(VfsError::IOError)?;
            runs.push(Run {
                vcn: abs,
                lcn,
                len: length,
            });
        }

        pos += len_len + off_len;
    }

    Ok(RunList { runs })
}

/// Read `buf.len()` bytes of a file described by `runs` starting at `offset`.
/// Sparse runs zero-fill; regions past the last run read as short.  Returns
/// the number of bytes actually read.
pub(crate) fn read_file_at(
    device: &dyn BlockDevice,
    boot: &BootSector,
    runs: &RunList,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, VfsError> {
    let cluster_size = boot.cluster_size();
    let mut pos = offset;
    let mut done = 0usize;

    while done < buf.len() {
        let Some(run) = runs.find(pos, cluster_size) else {
            break;
        };
        let run_start = run.vcn.checked_mul(cluster_size).ok_or(VfsError::IOError)?;
        let in_run = pos.checked_sub(run_start).ok_or(VfsError::IOError)?;
        // A crafted run length may be near i64::MAX; never let the byte-span
        // multiply wrap (a debug overflow check would abort the kernel).
        let avail = run
            .len
            .checked_mul(cluster_size)
            .ok_or(VfsError::IOError)?
            .saturating_sub(in_run);
        let want =
            core::cmp::min(avail, (buf.len() - done) as u64).min(MAX_READ_BYTES as u64) as usize;

        if run.lcn < 0 {
            buf[done..done + want].fill(0);
        } else {
            let lba = run
                .lcn
                .checked_mul(boot.sectors_per_cluster as i64)
                .and_then(|v| v.checked_add((in_run / boot.bytes_per_sector) as i64))
                .filter(|&v| v >= 0)
                .map(|v| v as u64)
                .ok_or(VfsError::IOError)?;
            let sector_off = (in_run % boot.bytes_per_sector) as usize;
            let total = sector_off + want;
            let secs = total.div_ceil(boot.bytes_per_sector as usize);
            let secs_u64 = secs as u64;
            if lba >= boot.total_sectors
                || secs_u64 > boot.total_sectors.saturating_sub(lba)
            {
                return Err(VfsError::IOError);
            }
            let mut tmp = vec![0u8; secs * boot.bytes_per_sector as usize];
            read_sectors(device, lba, secs as u32, &mut tmp)?;
            buf[done..done + want].copy_from_slice(&tmp[sector_off..sector_off + want]);
        }

        done += want;
        pos = pos.checked_add(want as u64).ok_or(VfsError::IOError)?;
    }

    Ok(done)
}
