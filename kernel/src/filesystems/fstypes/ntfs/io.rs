use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoRequest};
use crate::filesystems::vfs::error::VfsError;

pub(crate) fn read_sectors(
    device: &dyn BlockDevice,
    lba: u64,
    count: u32,
    buf: &mut [u8],
) -> Result<(), VfsError> {
    let req = IoRequest {
        lba,
        count,
        buffer: IoBuffer::Buf(buf),
        is_write: false,
    };
    let c = device.submit(&[req]).map_err(|e| {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("sector read: submit failed");
        let _ = e;
        VfsError::IOError
    })?;
    if !c.all_ok() {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("sector read: io completion error");
        return Err(VfsError::IOError);
    }
    Ok(())
}
