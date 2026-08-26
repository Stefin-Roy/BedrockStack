use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoRequest};
use crate::filesystems::vfs::error::VfsError;

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

pub(super) fn read_sectors(
    device: &dyn BlockDevice,
    lba: u64,
    count: u32,
    buf: &mut [u8],
) -> Result<(), VfsError> {
    fat_trace!({
        use core::fmt::Write;
        let mut port = crate::drivers::serial::SerialPort::new();
        write!(port, "[DBG:io] read lba=0x{:x} count={}\n", lba, count).ok();
    });
    let req = IoRequest {
        lba,
        count,
        buffer: IoBuffer::Buf(buf),
        is_write: false,
    };
    let c = device.submit(&[req]).map_err(|_| {
        crate::drivers::serial::SerialPort::puts("[fat32] read_sectors submit err lba=");
        crate::drivers::serial::SerialPort::put_hex(lba);
        crate::drivers::serial::SerialPort::puts("\n");
        VfsError::IOError
    })?;
    if !c.all_ok() {
        crate::drivers::serial::SerialPort::puts("[fat32] read_sectors !all_ok lba=");
        crate::drivers::serial::SerialPort::put_hex(lba);
        crate::drivers::serial::SerialPort::puts(" completed=");
        crate::drivers::serial::SerialPort::put_hex(c.completed as u64);
        crate::drivers::serial::SerialPort::puts(" errors=");
        crate::drivers::serial::SerialPort::put_hex(c.errors as u64);
        crate::drivers::serial::SerialPort::puts("\n");
        return Err(VfsError::IOError);
    }
    Ok(())
}

pub(super) fn write_sectors(
    device: &dyn BlockDevice,
    lba: u64,
    count: u32,
    buf: &[u8],
) -> Result<(), VfsError> {
    fat_trace!({
        use core::fmt::Write;
        let mut port = crate::drivers::serial::SerialPort::new();
        write!(port, "[DBG:io] write lba=0x{:x} count={}\n", lba, count).ok();
    });
    let req = IoRequest {
        lba,
        count,
        buffer: IoBuffer::ConstBuf(buf),
        is_write: true,
    };
    let c = device.submit(&[req]).map_err(|_| {
        crate::drivers::serial::SerialPort::puts("[fat32] write_sectors submit err lba=");
        crate::drivers::serial::SerialPort::put_hex(lba);
        crate::drivers::serial::SerialPort::puts("\n");
        VfsError::IOError
    })?;
    if !c.all_ok() {
        crate::drivers::serial::SerialPort::puts("[fat32] write_sectors !all_ok lba=");
        crate::drivers::serial::SerialPort::put_hex(lba);
        crate::drivers::serial::SerialPort::puts(" completed=");
        crate::drivers::serial::SerialPort::put_hex(c.completed as u64);
        crate::drivers::serial::SerialPort::puts(" errors=");
        crate::drivers::serial::SerialPort::put_hex(c.errors as u64);
        crate::drivers::serial::SerialPort::puts("\n");
        return Err(VfsError::IOError);
    }
    Ok(())
}

/// Batched multi-sector reads: each entry is (&mut [u8] buf, lba, count).
/// All `count`s must be sector-aligned and within PRDT limits; helper
/// collapses up to 16 entries into one AHCI NCQ submit to avoid per-run
/// IRQ + PRDT build overhead. Buffers may be disjoint slices of a larger
/// allocation (e.g. the caller's 28 MiB WAD buf) — helper reborrows via
/// raw parts to allow overlapping-borrow of disjoint slices.
pub(super) fn read_sectors_batch(
    device: &dyn BlockDevice,
    reqs: &mut [(&mut [u8], u64, u32)],
) -> Result<(), VfsError> {
    if reqs.is_empty() {
        return Ok(());
    }
    // Re-borrow each &mut [u8] via raw parts so disjoint slices of a single
    // backing allocation can be submitted together without borrow checker
    // treating them as overlapping.
    let mut io_reqs: alloc::vec::Vec<IoRequest> = alloc::vec::Vec::with_capacity(reqs.len());
    for (buf, lba, count) in reqs.iter_mut() {
        let ptr = buf.as_mut_ptr();
        let len = buf.len();
        let s = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        io_reqs.push(IoRequest {
            lba: *lba,
            count: *count,
            buffer: IoBuffer::Buf(s),
            is_write: false,
        });
    }
    let c = device.submit(&io_reqs).map_err(|_| {
        crate::drivers::serial::SerialPort::puts("[fat32] read_batch submit err\n");
        VfsError::IOError
    })?;
    if !c.all_ok() {
        crate::drivers::serial::SerialPort::puts("[fat32] read_batch !all_ok\n");
        return Err(VfsError::IOError);
    }
    Ok(())
}

/// Batched multi-sector writes: each entry is (&[u8] buf, lba, count).
/// Mirrors `read_sectors_batch` but for `ConstBuf` writes. Used to collapse
/// multiple contiguous-cluster writes (e.g. 28 MiB file create) into one NCQ
/// submit.
pub(super) fn write_sectors_batch(
    device: &dyn BlockDevice,
    reqs: &[(&[u8], u64, u32)],
) -> Result<(), VfsError> {
    if reqs.is_empty() {
        return Ok(());
    }
    let mut io_reqs: alloc::vec::Vec<IoRequest> = alloc::vec::Vec::with_capacity(reqs.len());
    for (buf, lba, count) in reqs.iter() {
        io_reqs.push(IoRequest {
            lba: *lba,
            count: *count,
            buffer: IoBuffer::ConstBuf(buf),
            is_write: true,
        });
    }
    let c = device.submit(&io_reqs).map_err(|_| {
        crate::drivers::serial::SerialPort::puts("[fat32] write_batch submit err\n");
        VfsError::IOError
    })?;
    if !c.all_ok() {
        crate::drivers::serial::SerialPort::puts("[fat32] write_batch !all_ok\n");
        return Err(VfsError::IOError);
    }
    Ok(())
}
