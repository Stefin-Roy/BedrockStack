use alloc::vec;

use crate::filesystems::vfs::error::VfsError;

use super::io::{read_sectors, write_sectors};

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

use super::mount::Fat32SuperBlock;
use super::fat::EOC_MARKER;

pub(super) fn read_cluster(sb: &Fat32SuperBlock, cluster: u32, buf: &mut [u8]) -> Result<(), VfsError> {
    let lba = sb.bpb.cluster_to_lba(cluster);
    fat_trace!({
        crate::drivers::serial::dump_puts("[DBG:fat32] read_cluster clus=0x");
        crate::drivers::serial::dump_put_hex(cluster as u64);
        crate::drivers::serial::dump_puts(" lba=0x");
        crate::drivers::serial::dump_put_hex(lba);
        crate::drivers::serial::dump_puts("\n");
    });
    read_sectors(&*sb.device, lba, sb.bpb.sec_per_clus as u32, buf)
}

pub(super) fn write_cluster(sb: &Fat32SuperBlock, cluster: u32, buf: &[u8]) -> Result<(), VfsError> {
    let lba = sb.bpb.cluster_to_lba(cluster);
    write_sectors(&*sb.device, lba, sb.bpb.sec_per_clus as u32, buf)
}

pub(super) fn zero_cluster(sb: &Fat32SuperBlock, cluster: u32) -> Result<(), VfsError> {
    let zeros = vec![0u8; sb.bpb.byts_per_clus as usize];
    write_cluster(sb, cluster, &zeros)
}

impl Fat32SuperBlock {
    pub fn chain_len(&self, start: u32) -> Result<u32, VfsError> {
        if start == 0 || start >= EOC_MARKER { return Ok(0); }
        let mut n = 1u32;
        let mut c = start;
        loop {
            let next = self.read_fat_entry(c)?;
            if next >= EOC_MARKER { break; }
            c = next;
            n += 1;
            if n > self.bpb.total_clus + 2 {
                return Err(VfsError::IOError);
            }
        }
        Ok(n)
    }

    pub fn extend_chain(&self, start: u32, additional: u32) -> Result<(), VfsError> {
        let mut tail = start;
        let mut _iters = 0u32;
        loop {
            let next = self.read_fat_entry(tail)?;
            if next >= EOC_MARKER { break; }
            tail = next;
            _iters += 1;
            if _iters > self.bpb.total_clus + 2 {
                return Err(VfsError::IOError);
            }
        }
        for _ in 0..additional {
            let new = self.alloc_cluster()?;
            zero_cluster(self, new)?;
            self.write_fat_entry(tail, new)?;
            tail = new;
        }
        Ok(())
    }

    pub fn chain_cluster_at(&self, start: u32, index: u32) -> Result<u32, VfsError> {
        let mut current = start;
        for _ in 0..index {
            current = self.read_fat_entry(current)?;
            if current >= EOC_MARKER { return Err(VfsError::IOError); }
        }
        Ok(current)
    }

    pub fn truncate_chain(&self, start: u32, keep: u32) -> Result<(), VfsError> {
        if start == 0 || keep == 0 {
            self.free_chain(start)?;
            return Ok(());
        }
        let mut c = start;
        for _ in 0..keep - 1 {
            let next = self.read_fat_entry(c)?;
            if next >= EOC_MARKER { return Ok(()); }
            c = next;
        }
        let next = self.read_fat_entry(c)?;
        if next < EOC_MARKER {
            self.write_fat_entry(c, EOC_MARKER)?;
            self.free_chain(next)?;
        }
        Ok(())
    }
}