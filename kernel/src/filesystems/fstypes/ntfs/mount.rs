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
fn boot_mft_runs(
    device: &dyn BlockDevice,
    boot: &BootSector,
) -> Result<RunList, VfsError> {
    let lba = boot.cluster_to_lba(boot.mft_lcn);
    let secs = boot.record_size / boot.bytes_per_sector;
    let mut buf = vec![0u8; boot.record_size as usize];
    super::io::read_sectors(device, lba, secs as u32, &mut buf)?;
    usa_fixup(&mut buf, boot.bytes_per_sector as usize)?;
    if buf.len() < 4
        || u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != super::record::RECORD_MAGIC
    {
        return Err(VfsError::IOError);
    }

    let attrs = iter_attrs(&buf)?;
    for a in attrs.iter() {
        if a.attr_type == ATTR_DATA && a.name.is_none() && !a.resident {
            let pairs = buf.get(a.map_off..a.map_end).ok_or(VfsError::IOError)?;
            return decode_mapping_pairs(pairs, a.lowest_vcn);
        }
    }
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
        let dev = device.ok_or(VfsError::InvalidDevice)?;
        let cached = CachedDevice::new(dev.clone());

        let boot = BootSector::parse(&*cached)?;
        let mft_runs = boot_mft_runs(&*cached, &boot)?;
        let sb = Arc::new(NtfsSuperBlock {
            device: cached,
            boot,
            mft_runs,
        });

        // The volume root is MFT record 5 ($Root).
        let root_ops = NtfsInode::load(sb.clone(), 5)?;
        if root_ops.file_type() != FileType::Directory {
            return Err(VfsError::IOError);
        }

        let root_inode = Arc::new(Inode::new(root_ops.clone()));
        let sb_vfs = Arc::new(SuperBlock::new(sb.clone() as Arc<dyn SuperOps>, root_inode));
        Ok((sb_vfs, root_ops))
    }
}