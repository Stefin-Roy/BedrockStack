use alloc::sync::Arc;
use alloc::vec;

use crate::filesystems::blockdriver::block_cache::CachedDevice;
use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::fstypes::FileSystem;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::{Inode, InodeOps};
use crate::filesystems::vfs::superblock::{StatFs, SuperBlock, SuperOps};
use crate::filesystems::vfs::types::FileType;

use super::attr::{ATTR_DATA, iter_attrs};
use super::boot::BootSector;
use super::inode::NtfsInode;
use super::record::usa_fixup;
use super::runs::{RunList, decode_mapping_pairs};

/// The read-only NTFS volume.  `mft_runs` is the run list of the $MFT file
/// itself (decoded from MFT record 0), used to locate every other record.
pub struct NtfsSuperBlock {
    pub(crate) device: Arc<CachedDevice>,
    pub(crate) boot: BootSector,
    pub(crate) mft_runs: RunList,
}

/// Extract the unnamed non-resident $DATA run list of MFT record 0 (the
/// $MFT).  Only used during bootstrap, before the superblock exists.
fn boot_mft_runs(device: &dyn BlockDevice, boot: &BootSector) -> Result<RunList, VfsError> {
    let lba = boot.cluster_to_lba(boot.mft_lcn);
    let secs = boot.record_size / boot.bytes_per_sector;
    let mut buf = vec![0u8; boot.record_size as usize];
    super::io::read_sectors(device, lba, secs as u32, &mut buf).map_err(|e| {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: boot_mft read_sectors failed");
        e
    })?;
    usa_fixup(&mut buf, boot.bytes_per_sector as usize).map_err(|e| {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: usa_fixup failed (torn write/USN mismatch)");
        e
    })?;
    if buf.len() < 4
        || u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != super::record::RECORD_MAGIC
    {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: MFT FILE magic miss (not 0x454C4946)");
        return Err(VfsError::IOError);
    }

    let attrs = iter_attrs(&buf).map_err(|e| {
        crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: MFT attr parse failed");
        e
    })?;
    for a in attrs.iter() {
        if a.attr_type == ATTR_DATA && a.name.is_none() && !a.resident {
            let pairs = buf.get(a.map_off..a.map_end).ok_or_else(|| {
                crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: MFT DATA mapping pairs OOB");
                VfsError::IOError
            })?;
            return decode_mapping_pairs(pairs, a.lowest_vcn).map_err(|e| {
                crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: MFT DATA run decode failed");
                e
            });
        }
    }
    crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: MFT DATA run not found (no non-resident $DATA)");
    Err(VfsError::IOError)
}

impl SuperOps for NtfsSuperBlock {
    fn statfs(&self) -> Result<StatFs, VfsError> {
        Ok(StatFs {
            block_size: self.boot.cluster_size() as u32,
            total_blocks: self.boot.total_sectors / self.boot.sectors_per_cluster as u64,
            free_blocks: 0,
        })
    }

    fn sync_fs(&self) -> Result<(), VfsError> {
        // Read-only: nothing to flush.
        Ok(())
    }
}

pub struct NtfsFileSystem;

impl FileSystem for NtfsFileSystem {
    fn name(&self) -> &str {
        "ntfs"
    }

    fn mount(
        &self,
        device: Option<Arc<dyn BlockDevice>>,
    ) -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError> {
        // Clear stale detail before each mount attempt.
        crate::filesystems::fstypes::ntfs::set_last_error(None);
        let dev = device.ok_or_else(|| {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: no device");
            VfsError::InvalidDevice
        })?;
        let cached = CachedDevice::new(dev.clone());

        let boot = BootSector::parse(&*cached).map_err(|e| {
            // BootSector returns InvalidDevice/InvalidInput for header checks,
            // keep those as-is; only IOError needs extra detail (already set via io.rs).
            if e == VfsError::IOError {
                crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: boot sector read/parse IOError");
            }
            e
        })?;
        let mft_runs = boot_mft_runs(&*cached, &boot)?;
        let sb = Arc::new(NtfsSuperBlock {
            device: cached,
            boot,
            mft_runs,
        });

        // The volume root is MFT record 5 ($Root).
        let root_ops = NtfsInode::load(sb.clone(), 5).map_err(|e| {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: load root MFT 5 failed");
            e
        })?;
        if root_ops.file_type() != FileType::Directory {
            crate::filesystems::fstypes::ntfs::set_last_error_if_none("ntfs: root MFT 5 not a directory");
            return Err(VfsError::IOError);
        }
        // Success: clear detail so subsequent unrelated IOError is not mis-attributed.
        crate::filesystems::fstypes::ntfs::set_last_error(None);

        let root_inode = Arc::new(Inode::new(root_ops.clone()));
        let sb_vfs = Arc::new(SuperBlock::new(sb.clone() as Arc<dyn SuperOps>, root_inode));
        Ok((sb_vfs, root_ops))
    }
}
