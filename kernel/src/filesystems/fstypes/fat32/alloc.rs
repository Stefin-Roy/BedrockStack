use core::sync::atomic::Ordering;

use crate::filesystems::vfs::error::VfsError;

use super::bpb::SECTOR_SIZE;
use super::fat::{EOC_MARKER, FREE_CLUSTER, FSINFO_LEAD_SIG, FSINFO_STRUCT_SIG, FSINFO_TRAIL_SIG};
use super::io::read_sectors;

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

use super::mount::Fat32SuperBlock;

impl Fat32SuperBlock {
    pub fn alloc_cluster(&self) -> Result<u32, VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] alloc_cluster\n"
        ));
        let mut hint = self.next_alloc_hint.lock();
        let n = self.bpb.total_clus;
        for i in 0..n {
            let clus = 2 + ((*hint - 2 + i) % n);
            if self.read_fat_entry(clus)? == FREE_CLUSTER {
                self.write_fat_entry(clus, EOC_MARKER)?;
                *hint = clus + 1;
                self.free_clus_count.fetch_sub(1, Ordering::Relaxed);
                return Ok(clus);
            }
        }
        Err(VfsError::NoSpace)
    }

    pub fn free_chain(&self, mut cluster: u32) -> Result<(), VfsError> {
        let mut _iters = 0u32;
        while cluster >= 2 && cluster < EOC_MARKER {
            let next = self.read_fat_entry(cluster)?;
            self.write_fat_entry(cluster, FREE_CLUSTER)?;
            self.free_clus_count.fetch_add(1, Ordering::Relaxed);
            cluster = next;
            _iters += 1;
            if _iters > self.bpb.total_clus + 2 {
                return Err(VfsError::IOError);
            }
        }
        Ok(())
    }

    pub fn scan_free_clusters(&self) -> Result<u32, VfsError> {
        let fat_idx = self.bpb.active_fat_idx();
        let first_lba = self.bpb.fat_sector_lba(fat_idx, 0);
        let nsec = self.bpb.fat_sz32;
        let total_clus = self.bpb.total_clus as u64;
        let mut count = 0u32;
        let mut buf = [0u8; SECTOR_SIZE];
        let first_valid = 2u64;
        let last_valid = first_valid + total_clus - 1;
        for sec in 0..nsec as u64 {
            read_sectors(&*self.device, first_lba + sec, 1, &mut buf)?;
            let entry_base = sec * 128;
            for i in 0..128 {
                let entry_idx = entry_base + i;
                if entry_idx < first_valid || entry_idx > last_valid {
                    continue;
                }
                let off = (i * 4) as usize;
                let val = u32::from_le_bytes(
                    buf.get(off..off + 4)
                        .ok_or(VfsError::IOError)?
                        .try_into()
                        .map_err(|_| VfsError::IOError)?,
                );
                if val & 0x0FFFFFFF == FREE_CLUSTER {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    pub fn write_fsinfo(&self) -> Result<(), VfsError> {
        if !self.bpb.fsinfo_is_valid() {
            return Ok(());
        }
        let sec = self.bpb.fsinfo_sec as u64;
        let mut buf = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, sec, 1, &mut buf)?;

        buf[0..4].copy_from_slice(&FSINFO_LEAD_SIG.to_le_bytes());
        buf[484..488].copy_from_slice(&FSINFO_STRUCT_SIG.to_le_bytes());
        buf[488..492].copy_from_slice(&self.free_clus_count.load(Ordering::Relaxed).to_le_bytes());
        buf[492..496].copy_from_slice(&(*self.next_alloc_hint.lock()).to_le_bytes());
        buf[508..512].copy_from_slice(&FSINFO_TRAIL_SIG.to_le_bytes());
        super::io::write_sectors(&*self.device, sec, 1, &buf)
    }
}
