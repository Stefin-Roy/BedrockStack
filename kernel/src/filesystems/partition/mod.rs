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

/// Last detailed probe/mount error for post-mortem logging (see `last_probe_error`).
/// Stores the `&'static str` returned by `probe` before it is collapsed to
/// `VfsError::InvalidDevice`, plus NTFS-stage detail when available.
static LAST_MOUNT_DETAIL: spin::Mutex<Option<&'static str>> = spin::Mutex::new(None);

pub fn last_mount_detail() -> Option<&'static str> {
    *LAST_MOUNT_DETAIL.lock()
}

pub fn last_probe_error() -> Option<&'static str> {
    last_mount_detail()
}

fn set_last_detail(s: Option<&'static str>) {
    *LAST_MOUNT_DETAIL.lock() = s;
}

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

    let has_protective = mbr[0x1C2] == 0xEE;

    if has_protective {
        if let Ok(parts) = gpt::parse(device.clone()) {
            return Ok(PartitionTable::Gpt(parts));
        }
    }

    let parts = mbr::parse(device, &mbr)?;
    Ok(PartitionTable::Mbr(parts))
}

pub fn mount_partition(
    device: Arc<dyn BlockDevice>,
    part_number: u32,
    fstype: &str,
    drive: char,
) -> Result<(), VfsError> {
    let table = match probe(device.clone()) {
        Ok(t) => {
            set_last_detail(None);
            t
        }
        Err(s) => {
            set_last_detail(Some(s));
            return Err(VfsError::InvalidDevice);
        }
    };
    let info = table
        .partitions()
        .iter()
        .find(|p| p.number == part_number && !p.is_extended)
        .ok_or_else(|| {
            set_last_detail(Some("partition number not found"));
            VfsError::NotFound
        })?;
    let part_dev = PartitionDevice::new(device, info);
    let res = vfs::mount(fstype, Some(Arc::new(part_dev)), drive);
    // Do not clear on failure: keep probe detail (if any) for lib.rs to log.
    // Success clears.
    if res.is_ok() {
        set_last_detail(None);
    }
    res
}

pub fn mount_first_partition(
    device: Arc<dyn BlockDevice>,
    fstype: &str,
    drive: char,
) -> Result<(), VfsError> {
    let table = match probe(device.clone()) {
        Ok(t) => {
            // Clear stale detail on successful probe; mount may still fail later.
            set_last_detail(None);
            t
        }
        Err(s) => {
            set_last_detail(Some(s));
            return Err(VfsError::InvalidDevice);
        }
    };
    let info = table
        .partitions()
        .iter()
        .find(|p| !p.is_extended)
        .ok_or_else(|| {
            set_last_detail(Some("no non-extended partition"));
            VfsError::NotFound
        })?;
    let part_dev = PartitionDevice::new(device, info);
    let res = vfs::mount(fstype, Some(Arc::new(part_dev)), drive);
    if res.is_ok() {
        set_last_detail(None);
    }
    res
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
