use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoCompletions, IoRequest};
use crate::filesystems::vfs;
use crate::filesystems::vfs::error::VfsError;

mod gpt;
mod mbr;

const SECTOR_SIZE: usize = 512;
const MAX_EBR_CHAIN: u32 = 100;

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub number: u32,
    pub start_lba: u64,
    pub end_lba: u64,
    pub size_sectors: u64,
    /// MBR partition type (byte 4 of a partition record).  `None` for GPT
    /// entries, whose type lives in [`Self::guid_type`].
    pub partition_type: Option<u8>,
    pub guid_type: Option<[u8; 16]>,
    pub guid_unique: Option<[u8; 16]>,
    pub name: Option<String>,
    pub is_extended: bool,
}

/// Mount attempt outcome carrying its own probe/mount detail string, so
/// callers can log precisely what failed without a racy process-global slot.
pub struct MountAttemptError {
    pub error: VfsError,
    pub detail: &'static str,
}

pub enum PartitionTable {
    Mbr(Vec<PartitionInfo>),
    Gpt(Vec<PartitionInfo>),
}

impl PartitionTable {
    pub fn partitions(&self) -> &[PartitionInfo] {
        match self {
            PartitionTable::Mbr(p) => p.as_slice(),
            PartitionTable::Gpt(p) => p.as_slice(),
        }
    }
}

pub struct PartitionDevice {
    inner: Arc<dyn BlockDevice>,
    start_lba: u64,
    sector_count: u64,
    model: String,
}

impl PartitionDevice {
    pub fn new(inner: Arc<dyn BlockDevice>, info: &PartitionInfo) -> Self {
        let model = format!("partition {} of {}", info.number, inner.model_string());
        PartitionDevice {
            inner,
            start_lba: info.start_lba,
            sector_count: info.size_sectors,
            model,
        }
    }
}

impl BlockDevice for PartitionDevice {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str> {
        let n = reqs.len();
        if n == 0 {
            return Ok(IoCompletions {
                completed: 0,
                errors: 0,
            });
        }

        let mut adjusted: Vec<IoRequest> = Vec::with_capacity(n);
        let partition_end = self
            .start_lba
            .checked_add(self.sector_count)
            .ok_or("partition range overflow")?;
        for r in reqs.iter() {
            let request_end = r
                .lba
                .checked_add(r.count as u64)
                .ok_or("partition request range overflow")?;
            if request_end > self.sector_count {
                return Err("partition LBA out of range");
            }
            let lba = self
                .start_lba
                .checked_add(r.lba)
                .ok_or("partition LBA overflow")?;
            if lba >= partition_end && r.count != 0 {
                return Err("partition LBA out of range");
            }
            let buffer = match &r.buffer {
                IoBuffer::Buf(buf) => {
                    let ptr = buf.as_ptr() as *mut u8;
                    let len = buf.len();
                    IoBuffer::Buf(unsafe { &mut *core::ptr::slice_from_raw_parts_mut(ptr, len) })
                }
                IoBuffer::ConstBuf(buf) => IoBuffer::ConstBuf(*buf),
                IoBuffer::Phys(pa, sz) => IoBuffer::Phys(*pa, *sz),
            };
            adjusted.push(IoRequest {
                lba,
                count: r.count,
                buffer,
                is_write: r.is_write,
            });
        }

        self.inner.submit(&adjusted)
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn model_string(&self) -> &str {
        &self.model
    }
}

pub fn probe(device: Arc<dyn BlockDevice>) -> Result<PartitionTable, &'static str> {
    let mut mbr = [0u8; 512];
    read_sector(&*device, 0, &mut mbr)?;

    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Err("no valid MBR or GPT signature");
    }

    // A protective MBR declares type 0xEE in any primary slot (hybrid disks
    // put it elsewhere than slot 0); check all four.
    let has_protective = (0..4).any(|i| mbr[0x1BE + i * 16 + 4] == 0xEE);

    if has_protective {
        match gpt::parse(device.clone()) {
            Ok(parts) => {
                check_overlaps(&parts)?;
                return Ok(PartitionTable::Gpt(parts));
            }
            // Corrupt GPT: fall through to MBR parsing, which now skips the
            // 0xEE stubs so no bogus "partition" leaks out.
            Err(gpt_err) => {
                let parts = mbr::parse(device, &mbr).map_err(|_| gpt_err)?;
                check_overlaps(&parts)?;
                return Ok(PartitionTable::Mbr(parts));
            }
        }
    }

    let parts = mbr::parse(device, &mbr)?;
    check_overlaps(&parts)?;
    Ok(PartitionTable::Mbr(parts))
}

/// Reject tables where two real partitions claim overlapping LBAs.
fn check_overlaps(parts: &[PartitionInfo]) -> Result<(), &'static str> {
    let mut sorted: Vec<&PartitionInfo> = parts
        .iter()
        .filter(|p| !p.is_extended && p.size_sectors > 0)
        .collect();
    sorted.sort_by_key(|p| p.start_lba);
    for w in sorted.windows(2) {
        let a = w[0];
        let b = w[1];
        let a_end = a
            .start_lba
            .checked_add(a.size_sectors)
            .ok_or("partition range overflow")?;
        if b.start_lba < a_end {
            return Err("overlapping partitions");
        }
    }
    Ok(())
}

/// Probe the table and mount `fstype` from the first non-extended partition
/// that successfully mounts.  Trying every candidate in order handles disks
/// whose ESP is not the first entry; the first success wins.
pub fn mount_first_partition(
    device: Arc<dyn BlockDevice>,
    fstype: &str,
    drive: char,
) -> Result<(), MountAttemptError> {
    let table = match probe(device.clone()) {
        Ok(t) => t,
        Err(s) => {
            return Err(MountAttemptError {
                error: VfsError::InvalidDevice,
                detail: s,
            });
        }
    };

    let candidates: Vec<&PartitionInfo> = table
        .partitions()
        .iter()
        .filter(|p| !p.is_extended && p.size_sectors > 0)
        .collect();

    if candidates.is_empty() {
        return Err(MountAttemptError {
            error: VfsError::NotFound,
            detail: "no non-extended partition",
        });
    }

    let mut last: Option<MountAttemptError> = None;
    for info in candidates {
        let part_dev = PartitionDevice::new(device.clone(), info);
        match vfs::mount(fstype, Some(Arc::new(part_dev)), drive) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Preserve the underlying VFS error for logging; use its
                // discriminant as the static detail so the caller can see
                // why the filesystem rejected this partition (e.g. InvalidDevice
                // vs NotFound) instead of a generic string.
                let detail: &'static str = e.discriminant_name();
                last = Some(MountAttemptError { error: e, detail });
            }
        }
    }
    Err(last.unwrap_or(MountAttemptError {
        error: VfsError::InvalidDevice,
        detail: "mount failed",
    }))
}

/// Mount a specific partition number.
pub fn mount_partition(
    device: Arc<dyn BlockDevice>,
    part_number: u32,
    fstype: &str,
    drive: char,
) -> Result<(), MountAttemptError> {
    let table = match probe(device.clone()) {
        Ok(t) => t,
        Err(s) => {
            return Err(MountAttemptError {
                error: VfsError::InvalidDevice,
                detail: s,
            });
        }
    };
    let info = table
        .partitions()
        .iter()
        .find(|p| p.number == part_number && !p.is_extended)
        .ok_or(MountAttemptError {
            error: VfsError::NotFound,
            detail: "partition number not found",
        })?;
    let part_dev = PartitionDevice::new(device, info);
    vfs::mount(fstype, Some(Arc::new(part_dev)), drive).map_err(|e| {
        let detail: &'static str = e.discriminant_name();
        MountAttemptError { error: e, detail }
    })
}

fn read_sector(device: &dyn BlockDevice, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    let req = IoRequest {
        lba,
        count: 1,
        buffer: IoBuffer::Buf(buf),
        is_write: false,
    };
    let c = device.submit(&[req])?;
    if !c.all_ok() {
        return Err("sector read error");
    }
    Ok(())
}

fn read_sectors(
    device: &dyn BlockDevice,
    lba: u64,
    count: u32,
    buf: &mut [u8],
) -> Result<(), &'static str> {
    let req = IoRequest {
        lba,
        count,
        buffer: IoBuffer::Buf(buf),
        is_write: false,
    };
    let c = device.submit(&[req])?;
    if !c.all_ok() {
        return Err("multi-sector read error");
    }
    Ok(())
}

fn crc32(buf: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in buf {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
