use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoRequest};
use crate::filesystems::vfs::error::VfsError;

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

pub(super) fn read_sectors(device: &dyn BlockDevice, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), VfsError> {
    fat_trace!({
        use core::fmt::Write;
        let mut port = crate::drivers::serial::SerialPort::new();
        write!(port, "[DBG:io] read lba=0x{:x} count={}\n", lba, count).ok();
    });
    let req = IoRequest { lba, count, buffer: IoBuffer::Buf(buf), is_write: false };
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

pub(super) fn write_sectors(device: &dyn BlockDevice, lba: u64, count: u32, buf: &[u8]) -> Result<(), VfsError> {
    fat_trace!({
        use core::fmt::Write;
        let mut port = crate::drivers::serial::SerialPort::new();
        write!(port, "[DBG:io] write lba=0x{:x} count={}\n", lba, count).ok();
    });
    let req = IoRequest { lba, count, buffer: IoBuffer::ConstBuf(buf), is_write: true };
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