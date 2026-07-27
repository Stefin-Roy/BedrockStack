use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};
use spin::Mutex;

use crate::filesystems::blockdriver::block_cache::CachedDevice;
use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoRequest};
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::superblock::{SuperBlock, SuperOps, StatFs};
use crate::filesystems::vfs::types::{DirEntry, FileType, Stat};
use super::FileSystem;

// ── Debug tracing ──────────────────────────────────────────────────────────

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

// ── FAT32 constants ──────────────────────────────────────────────────────────

const SECTOR_SIZE: usize = 512;
const DIR_ENTRY_SIZE: usize = 32;
const MAX_SFN_LEN: usize = 11;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = 0x0F;
const DIR_DELETED: u8 = 0xE5;
const DIR_END: u8 = 0x00;
const EOC_MARKER: u32 = 0x0FFFFFF8;
const FREE_CLUSTER: u32 = 0x00000000;

const FSINFO_LEAD_SIG: u32 = 0x41615252;
const FSINFO_STRUCT_SIG: u32 = 0x61417272;
const FSINFO_TRAIL_SIG: u32 = 0xAA550000;

// ── BPB (BIOS Parameter Block) ──────────────────────────────────────────────

#[derive(Clone)]
struct Bpb {
    bytes_per_sec: u16,
    sec_per_clus: u8,
    rsvd_sec_cnt: u16,
    num_fats: u8,
    fat_sz32: u32,
    root_clus: u32,
    fsinfo_sec: u16,
    byts_per_clus: u32,
    first_data_sec: u64,
    total_clus: u32,
    active_fat: u8,          // byte 0x42: bit 7=1 means single-FAT mode, bits 0-3 = active FAT
}

fn parse_bpb(device: &dyn BlockDevice) -> Result<Bpb, VfsError> {
    let mut sector = [0u8; SECTOR_SIZE];
    read_sectors(device, 0, 1, &mut sector)?;

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(VfsError::InvalidDevice);
    }

    let bytes_per_sec = u16::from_le_bytes([sector[0x0B], sector[0x0C]]);
    let sec_per_clus = sector[0x0D];
    let rsvd_sec_cnt = u16::from_le_bytes([sector[0x0E], sector[0x0F]]);
    let num_fats = sector[0x10];
    let root_ent_cnt = u16::from_le_bytes([sector[0x11], sector[0x12]]);
    let fat_sz16 = u16::from_le_bytes([sector[0x16], sector[0x17]]);
    let fat_sz32 = u32::from_le_bytes([sector[0x24], sector[0x25], sector[0x26], sector[0x27]]);
    let root_clus = u32::from_le_bytes([sector[0x2C], sector[0x2D], sector[0x2E], sector[0x2F]]);
    let fsinfo_sec = u16::from_le_bytes([sector[0x30], sector[0x31]]);

    // FAT32 discriminant: RootEntCnt must be 0 (no fixed root directory),
    // FATSz16 must be 0 (FAT32 uses the 32-bit size at 0x24),
    // and FATSz32 must be non-zero.
    if root_ent_cnt != 0 || fat_sz16 != 0 || fat_sz32 == 0 {
        return Err(VfsError::InvalidDevice);
    }

    if bytes_per_sec != SECTOR_SIZE as u16 {
        return Err(VfsError::InvalidInput);
    }
    if sec_per_clus == 0 || !sec_per_clus.is_power_of_two() || sec_per_clus > 128 {
        return Err(VfsError::InvalidInput);
    }
    if rsvd_sec_cnt == 0 {
        return Err(VfsError::InvalidInput);
    }
    if num_fats == 0 {
        return Err(VfsError::InvalidInput);
    }
    if root_clus < 2 {
        return Err(VfsError::InvalidInput);
    }

    let first_data_sec = rsvd_sec_cnt as u64 + (num_fats as u64) * fat_sz32 as u64;

    let total_sectors = {
        let sz16 = u16::from_le_bytes([sector[0x13], sector[0x14]]);
        if sz16 != 0 { sz16 as u64 } else {
            u32::from_le_bytes([sector[0x20], sector[0x21], sector[0x22], sector[0x23]]) as u64
        }
    };

    if total_sectors <= first_data_sec {
        return Err(VfsError::InvalidInput);
    }
    if total_sectors > device.sector_count() {
        return Err(VfsError::InvalidInput);
    }

    let total_data_sectors = total_sectors - first_data_sec;
    let total_clus = (total_data_sectors / sec_per_clus as u64) as u32;
    let byts_per_clus = (bytes_per_sec as u32) * (sec_per_clus as u32);

    let active_fat = sector[0x42];

    Ok(Bpb {
        bytes_per_sec, sec_per_clus, rsvd_sec_cnt, num_fats,
        fat_sz32, root_clus, fsinfo_sec,
        byts_per_clus, first_data_sec, total_clus, active_fat,
    })
}

impl Bpb {
    fn cluster_to_lba(&self, cluster: u32) -> u64 {
        let offset = (cluster.saturating_sub(2)) as u64;
        self.first_data_sec + offset * (self.sec_per_clus as u64)
    }

    fn active_fat_idx(&self) -> u8 {
        if self.active_fat & 0x80 != 0 { self.active_fat & 0x0F } else { 0 }
    }

    fn fat_sector_lba(&self, fat_num: u8, sector_idx: u32) -> u64 {
        self.rsvd_sec_cnt as u64
            + (fat_num as u64) * self.fat_sz32 as u64
            + sector_idx as u64
    }

    fn fat_entry_position(&self, cluster: u32) -> (u32, u32) {
        let byte_off = cluster as u32 * 4;
        if self.bytes_per_sec == 0 {
            crate::drivers::serial::dump_puts("[BPB] CORRUPT: bytes_per_sec=0 in fat_entry_position!\n");
            return (0, 0);
        }
        let sector_idx = byte_off / self.bytes_per_sec as u32;
        let offset = byte_off % self.bytes_per_sec as u32;
        (sector_idx, offset)
    }

    fn fsinfo_is_valid(&self) -> bool {
        self.fsinfo_sec != 0 && self.fsinfo_sec != 0xFFFF
    }
}

// ── FAT cache ───────────────────────────────────────────────────────────────

const MAX_FAT_CACHE_ENTRIES: usize = 4096;

struct FatCache {
    sectors: HashMap<u64, [u8; SECTOR_SIZE]>,
    dirty: HashSet<u64>,
    access_gen: HashMap<u64, u64>,
    gen_counter: u64,
}

impl FatCache {
    fn new() -> Self {
        FatCache {
            sectors: HashMap::new(),
            dirty: HashSet::new(),
            access_gen: HashMap::new(),
            gen_counter: 0,
        }
    }

    fn touch(&mut self, lba: u64) {
        let gen_val = self.gen_counter;
        self.gen_counter = gen_val.wrapping_add(1);
        self.access_gen.insert(lba, gen_val);
    }

    fn maybe_evict(&mut self) {
        if self.sectors.len() < MAX_FAT_CACHE_ENTRIES {
            return;
        }
        // Evict oldest clean entries down to 75% capacity
        let target = MAX_FAT_CACHE_ENTRIES - MAX_FAT_CACHE_ENTRIES / 4;
        let mut evictable: Vec<(u64, u64)> = self.sectors.keys()
            .filter_map(|lba| {
                if self.dirty.contains(lba) { return None; }
                Some((*lba, self.access_gen.get(lba).copied().unwrap_or(0)))
            })
            .collect();
        evictable.sort_by_key(|(_, g)| *g);
        let n_evict = self.sectors.len().saturating_sub(target);
        for (lba, _) in evictable.iter().take(n_evict) {
            self.sectors.remove(lba);
            self.access_gen.remove(lba);
        }
    }

    fn get_or_read(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&[u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict();
            self.sectors.insert(lba, buf);
        }
        self.touch(lba);
        Ok(self.sectors.get(&lba).unwrap())
    }

    fn get_or_read_mut(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&mut [u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.maybe_evict();
            self.sectors.insert(lba, buf);
        }
        self.dirty.insert(lba);
        self.touch(lba);
        Ok(self.sectors.get_mut(&lba).unwrap())
    }

    fn flush(&mut self, device: &dyn BlockDevice, bpb: &Bpb) -> Result<(), VfsError> {
        let is_mirrored = bpb.active_fat & 0x80 == 0;
        for &lba in self.dirty.iter() {
            let data = self.sectors.get(&lba).unwrap();
            write_sectors(device, lba, 1, data)?;
            if is_mirrored {
                for fat_num in 1..bpb.num_fats {
                    let local_idx = (lba - bpb.fat_sector_lba(0, 0)) % bpb.fat_sz32 as u64;
                    write_sectors(device, bpb.fat_sector_lba(fat_num, local_idx as u32), 1, data)?;
                }
            }
        }
        self.dirty.clear();
        Ok(())
    }
}

// ── Fat32SuperBlock (implements SuperOps) ────────────────────────────────────

pub struct Fat32SuperBlock {
    device: Arc<dyn BlockDevice>,
    bpb: Bpb,
    fat_cache: Mutex<FatCache>,
    next_ino: AtomicU64,
    next_alloc_hint: Mutex<u32>,
    free_clus_count: AtomicU32,
    volume_dirty: AtomicBool,
}

impl Fat32SuperBlock {
    fn read_fat_entry(&self, cluster: u32) -> Result<u32, VfsError> {
        let (sector_idx, offset) = self.bpb.fat_entry_position(cluster);
        let fat_idx = self.bpb.active_fat_idx();
        let lba = self.bpb.fat_sector_lba(fat_idx, sector_idx);
        let mut cache = self.fat_cache.lock();
        let sector = cache.get_or_read(&*self.device, lba)?;
        let val = u32::from_le_bytes([
            sector[offset as usize], sector[offset as usize + 1],
            sector[offset as usize + 2], sector[offset as usize + 3],
        ]);
        Ok(val & 0x0FFFFFFF)
    }

    fn write_fat_entry(&self, cluster: u32, value: u32) -> Result<(), VfsError> {
        let (sector_idx, offset) = self.bpb.fat_entry_position(cluster);
        let fat_idx = self.bpb.active_fat_idx();
        let lba = self.bpb.fat_sector_lba(fat_idx, sector_idx);
        let mut cache = self.fat_cache.lock();
        let sector = cache.get_or_read_mut(&*self.device, lba)?;
        let bytes = (value & 0x0FFFFFFF).to_le_bytes();
        sector[offset as usize..offset as usize + 4].copy_from_slice(&bytes);
        Ok(())
    }

    fn alloc_cluster(&self) -> Result<u32, VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] alloc_cluster\n"));
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

    fn free_chain(&self, mut cluster: u32) -> Result<(), VfsError> {
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

    fn chain_len(&self, start: u32) -> Result<u32, VfsError> {
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

    fn extend_chain(&self, start: u32, additional: u32) -> Result<(), VfsError> {
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

    fn chain_cluster_at(&self, start: u32, index: u32) -> Result<u32, VfsError> {
        let mut current = start;
        for _ in 0..index {
            current = self.read_fat_entry(current)?;
            if current >= EOC_MARKER { return Err(VfsError::IOError); }
        }
        Ok(current)
    }

    fn truncate_chain(&self, start: u32, keep: u32) -> Result<(), VfsError> {
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

    /// Read the entire active FAT into a contiguous buffer.
    ///
    /// Reads in chunks that fit within the AHCI PRDT limit (512 sectors per
    /// call), avoiding both the old per-sector flood and single-giant-read
    /// overflows.
    // 64 PRDT entries × 4 KiB/page = 256 KiB max per I/O, but the heap
    // allocator rarely gives page-aligned buffers so we lose one entry to
    // alignment.  504 sectors × 512 B = 252 KiB → fits in 63 full pages + 1
    // partial, well within 64 entries even at worst-case offset.
    const PRDT_MAX_SECTORS: u32 = 504;
    fn read_fat_bulk(&self) -> Result<Vec<u8>, VfsError> {
        let fat_idx = self.bpb.active_fat_idx();
        let first_lba = self.bpb.fat_sector_lba(fat_idx, 0);
        let nsec = self.bpb.fat_sz32;
        let mut fat = alloc::vec![0u8; nsec as usize * SECTOR_SIZE];
        let chunk = Self::PRDT_MAX_SECTORS;
        let mut done = 0u32;
        while done < nsec {
            let take = core::cmp::min(chunk, nsec - done);
            let off = done as usize * SECTOR_SIZE;
            read_sectors(&*self.device, first_lba + done as u64, take, &mut fat[off..])?;
            done += take;
        }
        Ok(fat)
    }

    fn scan_free_clusters(&self) -> Result<u32, VfsError> {
        let fat = self.read_fat_bulk()?;
        let n = self.bpb.total_clus;
        let mut count = 0u32;
        for c in 2..2 + n {
            let off = (c as usize) * 4;
            let val = u32::from_le_bytes(fat[off..off + 4].try_into().unwrap());
            if val & 0x0FFFFFFF == FREE_CLUSTER {
                count += 1;
            }
        }
        Ok(count)
    }

    fn write_fsinfo(&self) -> Result<(), VfsError> {
        if !self.bpb.fsinfo_is_valid() { return Ok(()); }
        let sec = self.bpb.fsinfo_sec as u64;
        let mut buf = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, sec, 1, &mut buf)?;

        buf[0..4].copy_from_slice(&FSINFO_LEAD_SIG.to_le_bytes());
        buf[484..488].copy_from_slice(&FSINFO_STRUCT_SIG.to_le_bytes());
        buf[488..492].copy_from_slice(&self.free_clus_count.load(Ordering::Relaxed).to_le_bytes());
        buf[492..496].copy_from_slice(&(*self.next_alloc_hint.lock()).to_le_bytes());
        buf[508..512].copy_from_slice(&FSINFO_TRAIL_SIG.to_le_bytes());
        write_sectors(&*self.device, sec, 1, &buf)
    }

    fn set_volume_dirty_flag(&self) -> Result<(), VfsError> {
        if self.volume_dirty.load(Ordering::Relaxed) { return Ok(()); }
        let mut sector = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, 0, 1, &mut sector)?;
        sector[0x41] |= 1;
        write_sectors(&*self.device, 0, 1, &sector)?;
        self.volume_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn clear_volume_dirty_flag(&self) -> Result<(), VfsError> {
        if !self.volume_dirty.load(Ordering::Relaxed) { return Ok(()); }
        let mut sector = [0u8; SECTOR_SIZE];
        read_sectors(&*self.device, 0, 1, &mut sector)?;
        sector[0x41] &= !1u8;
        write_sectors(&*self.device, 0, 1, &sector)?;
        self.volume_dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn flush_fat_cache(&self) -> Result<(), VfsError> {
        let mut cache = self.fat_cache.lock();
        cache.flush(&*self.device, &self.bpb)
    }

    fn sync_all(&self) -> Result<(), VfsError> {
        self.set_volume_dirty_flag()?;
        let mut cache = self.fat_cache.lock();
        cache.flush(&*self.device, &self.bpb)?;
        drop(cache);
        self.write_fsinfo()?;
        self.clear_volume_dirty_flag()
    }
}

impl SuperOps for Fat32SuperBlock {
    fn statfs(&self) -> Result<StatFs, VfsError> {
        Ok(StatFs {
            block_size: self.bpb.byts_per_clus,
            total_blocks: self.bpb.total_clus as u64,
            free_blocks: self.free_clus_count.load(Ordering::Relaxed) as u64,
        })
    }
    fn sync_fs(&self) -> Result<(), VfsError> {
        self.sync_all()
    }
}

// ── Sector/cluster I/O helpers ──────────────────────────────────────────────

fn read_sectors(device: &dyn BlockDevice, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), VfsError> {
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

fn write_sectors(device: &dyn BlockDevice, lba: u64, count: u32, buf: &[u8]) -> Result<(), VfsError> {
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

fn read_cluster(sb: &Fat32SuperBlock, cluster: u32, buf: &mut [u8]) -> Result<(), VfsError> {
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

fn write_cluster(sb: &Fat32SuperBlock, cluster: u32, buf: &[u8]) -> Result<(), VfsError> {
    let lba = sb.bpb.cluster_to_lba(cluster);
    write_sectors(&*sb.device, lba, sb.bpb.sec_per_clus as u32, buf)
}

fn zero_cluster(sb: &Fat32SuperBlock, cluster: u32) -> Result<(), VfsError> {
    let zeros = alloc::vec![0u8; sb.bpb.byts_per_clus as usize];
    write_cluster(sb, cluster, &zeros)
}

// ── Timestamp helpers (always 0 — no RTC) ────────────────────────────────────

fn set_timestamps(entry: &mut [u8; DIR_ENTRY_SIZE]) {
    entry[0x0D] = 0;
    entry[0x0E..0x10].copy_from_slice(&[0, 0]);
    entry[0x10..0x12].copy_from_slice(&[0, 0]);
    entry[0x12..0x14].copy_from_slice(&[0, 0]);
    entry[0x16..0x18].copy_from_slice(&[0, 0]);
    entry[0x18..0x1A].copy_from_slice(&[0, 0]);
}

// ── Name encoding/decoding ──────────────────────────────────────────────────

fn decode_sfn(sfn: &[u8; MAX_SFN_LEN]) -> String {
    let mut name = String::new();
    let stem_end = sfn[..8].iter().rposition(|&b| b != b' ').map(|p| p + 1).unwrap_or(0);
    for b in &sfn[..stem_end] {
        name.push((*b as char).to_ascii_lowercase());
    }
    let ext_start = sfn[8..11].iter().position(|&b| b == b' ').unwrap_or(3);
    if ext_start > 0 {
        name.push('.');
        for b in &sfn[8..8 + ext_start] {
            name.push((*b as char).to_ascii_lowercase());
        }
    }
    name
}

fn decode_volume_label(sfn: &[u8; MAX_SFN_LEN]) -> String {
    let end = sfn.iter().rposition(|&b| b != b' ').map(|p| p + 1).unwrap_or(0);
    core::str::from_utf8(&sfn[..end]).unwrap_or("").trim_end_matches('\0').to_string()
}

fn make_sfn_bytes(stem: &str, ext: &str) -> [u8; MAX_SFN_LEN] {
    let mut sfn = [b' '; MAX_SFN_LEN];
    for (i, &b) in stem.as_bytes().iter().enumerate() {
        if i >= 8 { break; }
        sfn[i] = b.to_ascii_uppercase();
    }
    for (i, &b) in ext.as_bytes().iter().enumerate() {
        if i >= 3 { break; }
        sfn[8 + i] = b.to_ascii_uppercase();
    }
    sfn
}

fn sfn_from_name(name: &str, existing_sfns: &HashSet<[u8; MAX_SFN_LEN]>) -> Option<[u8; MAX_SFN_LEN]> {
    if name.is_empty() { return None; }
    let (stem, ext) = if let Some(dot) = name.rfind('.') {
        if dot == 0 { ("", &name[1..]) } else { (&name[..dot], &name[dot + 1..]) }
    } else {
        (name, "")
    };

    let base = make_sfn_bytes(stem, ext);
    if !existing_sfns.contains(&base) {
        return Some(base);
    }

    let mut counter = 1u32;
    loop {
        let suffix = alloc::format!("~{}", counter);
        let suffix_bytes = suffix.as_bytes();
        if suffix_bytes.len() > 6 { return None; }
        let stem_avail = 8 - suffix_bytes.len();
        let stem_trunc = &stem.as_bytes()[..stem.len().min(stem_avail)];
        let mut sfn = [b' '; MAX_SFN_LEN];
        for (i, &b) in stem_trunc.iter().enumerate() {
            sfn[i] = b.to_ascii_uppercase();
        }
        for (j, &b) in suffix_bytes.iter().enumerate() {
            sfn[stem_avail + j] = b;
        }
        for (i, &b) in ext.as_bytes().iter().enumerate() {
            if i >= 3 { break; }
            sfn[8 + i] = b.to_ascii_uppercase();
        }
        if !existing_sfns.contains(&sfn) {
            return Some(sfn);
        }
        counter += 1;
        if counter > 99999 { return None; }
    }
}

fn vfat_checksum(sfn: &[u8; MAX_SFN_LEN]) -> u8 {
    let mut sum: u8 = 0;
    for i in 0..MAX_SFN_LEN {
        sum = ((sum >> 1) | (sum << 7)).wrapping_add(sfn[i]);
    }
    sum
}

fn needs_vfat(name: &str) -> bool {
    if name == "." || name == ".." { return false; }
    let dot = name.rfind('.');
    let base_len = dot.unwrap_or(name.len());
    let ext_len = if let Some(d) = dot { name.len() - d - 1 } else { 0 };
    base_len > 8 || ext_len > 3 || name.bytes().any(|b| b > 127 || b == b' ')
}

fn decode_vfat_name(entries: &[[u8; DIR_ENTRY_SIZE]]) -> String {
    let mut utf16_buf: Vec<u16> = Vec::new();
    for entry in entries.iter().rev() {
        if entry[0] == DIR_DELETED || entry[0] & 0x1F == 0 { continue; }
        for j in 0..13 {
            let c = get_vfat_char(entry, j);
            if c == 0 || c == 0xFFFF { break; }
            utf16_buf.push(c);
        }
    }
    String::from_utf16_lossy(&utf16_buf)
}

fn get_vfat_char(entry: &[u8; DIR_ENTRY_SIZE], index: usize) -> u16 {
    match index {
        0..=4   => u16::from_le_bytes([entry[1 + index * 2], entry[2 + index * 2]]),
        5..=10  => u16::from_le_bytes([entry[14 + (index - 5) * 2], entry[15 + (index - 5) * 2]]),
        11..=12 => u16::from_le_bytes([entry[28 + (index - 11) * 2], entry[29 + (index - 11) * 2]]),
        _ => 0,
    }
}

fn set_vfat_char(entry: &mut [u8; DIR_ENTRY_SIZE], index: usize, c: u16) {
    let bytes = c.to_le_bytes();
    match index {
        0..=4   => { entry[1 + index * 2] = bytes[0]; entry[2 + index * 2] = bytes[1]; }
        5..=10  => { entry[14 + (index - 5) * 2] = bytes[0]; entry[15 + (index - 5) * 2] = bytes[1]; }
        11..=12 => { entry[28 + (index - 11) * 2] = bytes[0]; entry[29 + (index - 11) * 2] = bytes[1]; }
        _ => {}
    }
}

fn encode_vfat_entries(name: &str, checksum: u8) -> Vec<[u8; DIR_ENTRY_SIZE]> {
    let u16_chars: Vec<u16> = name.encode_utf16().collect();
    let needed = (u16_chars.len() + 12) / 13;
    let mut entries = Vec::with_capacity(needed);
    for i in 0..needed {
        let mut entry = [0u8; DIR_ENTRY_SIZE];
        let start = i * 13;
        let count = (u16_chars.len() - start).min(13);
        let ord = (needed - i) as u8;
        entry[0] = if i == 0 { ord | 0x40 } else { ord };
        entry[11] = ATTR_LONG_NAME;
        entry[12] = 0;
        entry[13] = checksum;
        for j in 0..count { set_vfat_char(&mut entry, j, u16_chars[start + j]); }
        for j in count..13 { set_vfat_char(&mut entry, j, 0xFFFF); }
        entries.push(entry);
    }
    // VFAT requires LFN entries in directory order: FIRST entry (ord=1) farthest
    // from SFN, LAST entry (ord=N|0x40) adjacent to SFN.
    entries.reverse();
    entries
}

// ── Directory entry helpers ─────────────────────────────────────────────────

fn first_clus_from_entry(entry: &[u8; DIR_ENTRY_SIZE]) -> u32 {
    let hi = u16::from_le_bytes([entry[0x14], entry[0x15]]);
    let lo = u16::from_le_bytes([entry[0x1A], entry[0x1B]]);
    (hi as u32) << 16 | lo as u32
}

fn set_first_clus_in_entry(entry: &mut [u8; DIR_ENTRY_SIZE], cluster: u32) {
    let lo_bytes = (cluster as u16).to_le_bytes();
    let hi_bytes = ((cluster >> 16) as u16).to_le_bytes();
    entry[0x14] = hi_bytes[0]; entry[0x15] = hi_bytes[1];
    entry[0x1A] = lo_bytes[0]; entry[0x1B] = lo_bytes[1];
}

fn file_size_from_entry(entry: &[u8; DIR_ENTRY_SIZE]) -> u32 {
    u32::from_le_bytes([entry[0x1C], entry[0x1D], entry[0x1E], entry[0x1F]])
}

fn set_file_size_in_entry(entry: &mut [u8; DIR_ENTRY_SIZE], size: u32) {
    let bytes = size.to_le_bytes();
    entry[0x1C] = bytes[0]; entry[0x1D] = bytes[1];
    entry[0x1E] = bytes[2]; entry[0x1F] = bytes[3];
}

// ── Directory reading ───────────────────────────────────────────────────────

#[derive(Clone)]
struct DirEntrySlot {
    vfat_entries: Vec<[u8; DIR_ENTRY_SIZE]>,
    sfn_entry: [u8; DIR_ENTRY_SIZE],
}

fn read_dir_slots(sb: &Fat32SuperBlock, dir_clus: u32) -> Result<Vec<DirEntrySlot>, VfsError> {
    if sb.bpb.byts_per_clus == 0 || sb.bpb.bytes_per_sec == 0 || sb.bpb.sec_per_clus == 0 {
        crate::drivers::serial::dump_puts("[FAT32] CORRUPT BPB in read_dir_slots!\n");
        crate::drivers::serial::dump_puts("  byts_per_clus=");
        crate::drivers::serial::dump_put_hex(sb.bpb.byts_per_clus as u64);
        crate::drivers::serial::dump_puts(" bytes_per_sec=");
        crate::drivers::serial::dump_put_hex(sb.bpb.bytes_per_sec as u64);
        crate::drivers::serial::dump_puts(" sec_per_clus=");
        crate::drivers::serial::dump_put_hex(sb.bpb.sec_per_clus as u64);
        crate::drivers::serial::dump_puts("\n");
        return Err(VfsError::IOError);
    }
    let mut slots: Vec<DirEntrySlot> = Vec::new();
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut buf = alloc::vec![0u8; clus_bytes];
    let mut cluster = dir_clus;
    let mut vfat_chain: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
    let mut _iters = 0u32;
    let mut end_of_dir = false;

    fat_trace!({
        crate::drivers::serial::dump_puts("[DBG:fat32] read_dir_slots cluster=0x");
        crate::drivers::serial::dump_put_hex(cluster as u64);
        crate::drivers::serial::dump_puts("\n");
    });
    loop {
        read_cluster(sb, cluster, &mut buf)?;
        fat_trace!(crate::drivers::serial::dump_puts("[DBG:fat32] read_dir_slots read cluster ok\n"));
        for i in 0..entries_per_clus {
            let off = i * DIR_ENTRY_SIZE;
            let entry: &[u8; DIR_ENTRY_SIZE] = &buf[off..off + DIR_ENTRY_SIZE].try_into().unwrap();
            if entry[0] == DIR_END { vfat_chain.clear(); end_of_dir = true; break; }
            if entry[0] == DIR_DELETED { vfat_chain.clear(); continue; }
            let attr = entry[0x0B];
            if attr == ATTR_LONG_NAME { vfat_chain.push(*entry); continue; }
            // Keep volume labels so readdir can expose them
            if attr & ATTR_VOLUME_ID != 0 {
                vfat_chain.clear();
                slots.push(DirEntrySlot { vfat_entries: Vec::new(), sfn_entry: *entry });
                continue;
            }
            slots.push(DirEntrySlot { vfat_entries: core::mem::take(&mut vfat_chain), sfn_entry: *entry });
        }
        if end_of_dir { break; }
        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER { break; }
        cluster = next;
        _iters += 1;
        if _iters > sb.bpb.total_clus + 2 {
            return Err(VfsError::IOError);
        }
    }
    Ok(slots)
}

fn decode_entry_name(slot: &DirEntrySlot) -> String {
    let attr = slot.sfn_entry[0x0B];
    if attr & ATTR_VOLUME_ID != 0 {
        decode_volume_label(&slot.sfn_entry[..MAX_SFN_LEN].try_into().unwrap_or([b' '; MAX_SFN_LEN]))
    } else if !slot.vfat_entries.is_empty() {
        decode_vfat_name(&slot.vfat_entries)
    } else {
        decode_sfn(&slot.sfn_entry[..MAX_SFN_LEN].try_into().unwrap_or([b' '; MAX_SFN_LEN]))
    }
}

// ── Directory writing / updating helpers ────────────────────────────────────

fn write_dir_entries(sb: &Fat32SuperBlock, dir_clus: &u32,
                     entries: &[[u8; DIR_ENTRY_SIZE]]) -> Result<(), VfsError>
{
    fat_trace!({
        use core::fmt::Write;
        let mut port = crate::drivers::serial::SerialPort::new();
        write!(port, "[DBG:fat32] wde enter clus={} entries={}\n", *dir_clus, entries.len()).ok();
    });
    if entries.is_empty() { return Ok(()); }
    if *dir_clus < 2 { fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] wde bad clus\n")); return Err(VfsError::InvalidInput); }
    let total = entries.len();
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut placed = 0usize;
    let mut cluster = *dir_clus;
    let mut buf = alloc::vec![0u8; clus_bytes];
    let mut _iters = 0u32;

    loop {
        read_cluster(sb, cluster, &mut buf)?;
        let mut found_spot = false;

        for i in 0..entries_per_clus {
            let off = i * DIR_ENTRY_SIZE;
            let first = buf[off];
            if first == DIR_DELETED || first == DIR_END {
                let mut space = 1usize;
                if first == DIR_DELETED {
                    for j in (i + 1)..entries_per_clus {
                        let b = buf[j * DIR_ENTRY_SIZE];
                        if b == DIR_DELETED || b == DIR_END { space += 1; } else { break; }
                    }
                } else {
                    space = entries_per_clus - i;
                }
                let need = total - placed;
                if space >= need {
                    for j in 0..need {
                        buf[off + j * DIR_ENTRY_SIZE..off + (j + 1) * DIR_ENTRY_SIZE]
                            .copy_from_slice(&entries[placed + j]);
                    }
                    placed = total;
                    found_spot = true;
                    break;
                }
            }
        }

        if found_spot {
            write_cluster(sb, cluster, &buf)?;
            return Ok(());
        }

        _iters += 1;
        if _iters > sb.bpb.total_clus + 2 {
            return Err(VfsError::IOError);
        }
        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER {
            let new_clus = sb.alloc_cluster()?;
            zero_cluster(sb, new_clus)?;
            sb.write_fat_entry(cluster, new_clus)?;
            cluster = new_clus;
            let mut new_buf = alloc::vec![0u8; clus_bytes];
            for j in 0..(total - placed) {
                new_buf[j * DIR_ENTRY_SIZE..(j + 1) * DIR_ENTRY_SIZE]
                    .copy_from_slice(&entries[placed + j]);
            }
            write_cluster(sb, cluster, &new_buf)?;
            return Ok(());
        }
        cluster = next;
    }
}

fn find_and_update_entry<F>(sb: &Fat32SuperBlock, dir_clus: u32, name: &str,
                             mut f: F) -> Result<(), VfsError>
where
    F: FnMut(&mut [u8; DIR_ENTRY_SIZE]),
{
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut buf = alloc::vec![0u8; clus_bytes];

    let mut cluster = dir_clus;
    let mut _iters = 0u32;
    loop {
        read_cluster(sb, cluster, &mut buf)?;

        let mut i = 0;
        while i < entries_per_clus {
            let off = i * DIR_ENTRY_SIZE;
            let first = buf[off];
            if first == DIR_END { return Err(VfsError::NotFound); }
            if first == DIR_DELETED { i += 1; continue; }
            let attr = buf[off + 0x0B];
            if attr == ATTR_LONG_NAME { i += 1; continue; }
            if attr & ATTR_VOLUME_ID != 0 { i += 1; continue; }

            let mut chain_start = i;
            while chain_start > 0 && buf[(chain_start - 1) * DIR_ENTRY_SIZE + 0x0B] == ATTR_LONG_NAME {
                chain_start -= 1;
            }

            let mut vfat_buf: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
            for j in chain_start..i {
                let e: &[u8; DIR_ENTRY_SIZE] = &buf[j * DIR_ENTRY_SIZE..(j + 1) * DIR_ENTRY_SIZE].try_into().unwrap();
                vfat_buf.push(*e);
            }
            let entry_name = if !vfat_buf.is_empty() {
                decode_vfat_name(&vfat_buf)
            } else {
                let sfn: &[u8; MAX_SFN_LEN] = &buf[off..off + MAX_SFN_LEN].try_into().unwrap_or([b' '; MAX_SFN_LEN]);
                decode_sfn(sfn)
            };

            if entry_name.eq_ignore_ascii_case(name) {
                let mut sfn_entry: [u8; DIR_ENTRY_SIZE] = buf[off..off + DIR_ENTRY_SIZE].try_into().unwrap();
                f(&mut sfn_entry);
                buf[off..off + DIR_ENTRY_SIZE].copy_from_slice(&sfn_entry);
                write_cluster(sb, cluster, &buf)?;
                return Ok(());
            }
            i += 1;
        }

        _iters += 1;
        if _iters > sb.bpb.total_clus + 2 {
            return Err(VfsError::IOError);
        }
        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER { break; }
        cluster = next;
    }
    Err(VfsError::NotFound)
}

fn update_entry_cluster_and_size(sb: &Fat32SuperBlock, dir_clus: u32,
                                  name: &str, new_clus: Option<u32>,
                                  new_size: Option<u32>) -> Result<(), VfsError>
{
    find_and_update_entry(sb, dir_clus, name, |entry| {
        if let Some(c) = new_clus { set_first_clus_in_entry(entry, c); }
        if let Some(s) = new_size { set_file_size_in_entry(entry, s); }
        set_timestamps(entry);
    })
}

fn remove_dir_entries(sb: &Fat32SuperBlock, dir_clus: u32, name: &str) -> Result<(), VfsError> {
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut buf = alloc::vec![0u8; clus_bytes];

    let mut cluster = dir_clus;
    let mut _iters = 0u32;
    loop {
        read_cluster(sb, cluster, &mut buf)?;
        let mut i = 0;
        while i < entries_per_clus {
            let off = i * DIR_ENTRY_SIZE;
            let first = buf[off];
            if first == DIR_END { return Err(VfsError::NotFound); }
            if first == DIR_DELETED { i += 1; continue; }
            let attr = buf[off + 0x0B];
            if attr == ATTR_LONG_NAME { i += 1; continue; }
            if attr & ATTR_VOLUME_ID != 0 { i += 1; continue; }

            let mut chain_start = i;
            while chain_start > 0 && buf[(chain_start - 1) * DIR_ENTRY_SIZE + 0x0B] == ATTR_LONG_NAME {
                chain_start -= 1;
            }

            let mut vfat_buf: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
            for j in chain_start..i {
                let e = &buf[j * DIR_ENTRY_SIZE..(j + 1) * DIR_ENTRY_SIZE].try_into().unwrap();
                vfat_buf.push(*e);
            }
            let entry_name = if !vfat_buf.is_empty() {
                decode_vfat_name(&vfat_buf)
            } else {
                let sfn = &buf[off..off + MAX_SFN_LEN].try_into().unwrap_or([b' '; MAX_SFN_LEN]);
                decode_sfn(sfn)
            };

            if entry_name.eq_ignore_ascii_case(name) {
                for j in chain_start..=i {
                    buf[j * DIR_ENTRY_SIZE] = DIR_DELETED;
                }
                write_cluster(sb, cluster, &buf)?;
                return Ok(());
            }
            i += 1;
        }

        _iters += 1;
        if _iters > sb.bpb.total_clus + 2 {
            return Err(VfsError::IOError);
        }
        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER { break; }
        cluster = next;
    }
    Err(VfsError::NotFound)
}

// ── Fat32Inode (implements InodeOps) ────────────────────────────────────────

pub struct Fat32Inode {
    sb: Arc<Fat32SuperBlock>,
    first_clus: AtomicU32,
    size: AtomicU32,
    file_type: FileType,
    ino: u64,
    parent_clus: u32,
    entry_name: String,
    unlinked: AtomicBool,
    dir_cache: Mutex<Option<(u64, Vec<DirEntrySlot>)>>,
    dir_generation: AtomicU64,
    dir_lock: Mutex<()>,
    write_lock: Mutex<()>,
}

impl Drop for Fat32Inode {
    fn drop(&mut self) {
        if !self.unlinked.load(Ordering::Relaxed) {
            return;
        }
        let first = self.first_clus.load(Ordering::Relaxed);
        if first >= 2 && first < EOC_MARKER {
            let _ = self.sb.free_chain(first);
        }
    }
}

impl Fat32Inode {
    fn sync_clus_and_size(&self) -> Result<(), VfsError> {
        update_entry_cluster_and_size(
            &self.sb, self.parent_clus, &self.entry_name,
            Some(self.first_clus.load(Ordering::Relaxed)),
            Some(self.size.load(Ordering::Relaxed)),
        )
    }

    fn invalidate_dir_cache(&self) {
        self.dir_generation.fetch_add(1, Ordering::Relaxed);
    }

    fn get_dir_slots(&self) -> Result<Vec<DirEntrySlot>, VfsError> {
        let generation = self.dir_generation.load(Ordering::Relaxed);
        {
            let cache = self.dir_cache.lock();
            if let Some((g, ref slots)) = *cache {
                if g == generation {
                    return Ok(slots.clone());
                }
            }
        }
        fat_trace!(crate::drivers::serial::dump_puts("[DBG:fat32] get_dir_slots re-read from disk\n"));
        let first = self.first_clus.load(Ordering::Relaxed);
        fat_trace!({
            crate::drivers::serial::dump_puts("[DBG:fat32] get_dir_slots first_clus=0x");
            crate::drivers::serial::dump_put_hex(first as u64);
            crate::drivers::serial::dump_puts("\n");
        });
        let slots = read_dir_slots(&self.sb, first)?;
        fat_trace!({
            crate::drivers::serial::dump_puts("[DBG:fat32] get_dir_slots got ");
            crate::drivers::serial::dump_put_hex(slots.len() as u64);
            crate::drivers::serial::dump_puts(" slots\n");
        });
        *self.dir_cache.lock() = Some((generation, slots.clone()));
        Ok(slots)
    }
}

impl InodeOps for Fat32Inode {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.file_type != FileType::Regular { return Err(VfsError::IsADirectory); }
        let file_size = self.size.load(Ordering::Relaxed) as u64;
        let first = self.first_clus.load(Ordering::Relaxed);
        if offset >= file_size || buf.is_empty() || first == 0 { return Ok(0); }

        let clus_size = self.sb.bpb.byts_per_clus as u64;
        let total = (buf.len() as u64).min(file_size - offset) as usize;
        let start_idx = (offset / clus_size) as u32;
        let mut current = if start_idx == 0 {
            first
        } else {
            self.sb.chain_cluster_at(first, start_idx)?
        };

        if current >= EOC_MARKER || current < 2 { return Ok(0); }

        let clus_bytes = clus_size as usize;
        let mut cluster_buf = alloc::vec![0u8; clus_bytes];
        let mut done = 0usize;
        let mut clus_off = (offset % clus_size) as usize;
        let mut _rd_iters = 0u32;

        while done < total {
            read_cluster(&self.sb, current, &mut cluster_buf)?;
            let avail = clus_bytes - clus_off;
            let want = (total - done).min(avail);
            buf[done..done + want].copy_from_slice(&cluster_buf[clus_off..clus_off + want]);
            done += want;
            if done >= total { break; }
            current = self.sb.read_fat_entry(current)?;
            if current >= EOC_MARKER { break; }
            clus_off = 0;
            _rd_iters += 1;
            if _rd_iters > self.sb.bpb.total_clus + 2 {
                return Err(VfsError::IOError);
            }
        }
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        if self.file_type != FileType::Regular { return Err(VfsError::IsADirectory); }
        if buf.is_empty() { return Ok(0); }
        if offset.saturating_add(buf.len() as u64) > u32::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        let _write_lock = self.write_lock.lock();

        let clus_size = self.sb.bpb.byts_per_clus as u64;
        let end_byte = offset + buf.len() as u64;
        let needed_clus = if end_byte == 0 { 0 } else { ((end_byte - 1) / clus_size + 1) as u32 };

        let mut current_first_clus = self.first_clus.load(Ordering::Relaxed);
        if needed_clus > 0 {
            let have = self.sb.chain_len(current_first_clus)?;
            if have < needed_clus {
                if current_first_clus == 0 {
                    current_first_clus = self.sb.alloc_cluster()?;
                    zero_cluster(&self.sb, current_first_clus)?;
                    if needed_clus > 1 {
                        self.sb.extend_chain(current_first_clus, needed_clus - 1)?;
                    }
                    self.first_clus.store(current_first_clus, Ordering::Relaxed);
                    self.sync_clus_and_size()?;
                } else {
                    self.sb.extend_chain(current_first_clus, needed_clus - have)?;
                }
            }
        }

        let start_idx = (offset / clus_size) as u32;
        let mut current = if start_idx == 0 {
            current_first_clus
        } else {
            self.sb.chain_cluster_at(current_first_clus, start_idx)?
        };

        let clus_bytes = clus_size as usize;
        let mut cluster_buf = alloc::vec![0u8; clus_bytes];
        let mut done = 0usize;
        let mut clus_off = (offset % clus_size) as usize;
        let mut _wr_iters = 0u32;

        while done < buf.len() {
            let need_rmw = clus_off != 0
                || buf.len() - done < clus_bytes
                || (done > 0 && clus_off == 0);
            if need_rmw {
                read_cluster(&self.sb, current, &mut cluster_buf)?;
            } else {
                cluster_buf = alloc::vec![0u8; clus_bytes];
            }
            let avail = clus_bytes - clus_off;
            let want = (buf.len() - done).min(avail);
            cluster_buf[clus_off..clus_off + want].copy_from_slice(&buf[done..done + want]);
            write_cluster(&self.sb, current, &cluster_buf)?;
            done += want;
            if done >= buf.len() { break; }
            current = self.sb.read_fat_entry(current)?;
            if current >= EOC_MARKER { break; }
            clus_off = 0;
            _wr_iters += 1;
            if _wr_iters > self.sb.bpb.total_clus + 2 {
                return Err(VfsError::IOError);
            }
        }

        // Flush FAT changes to disk before updating directory entry.
        // Order: data → FAT flush → dir entry.  A crash before the flush
        // orphans only data; a crash after leaves a consistent chain.
        self.sb.flush_fat_cache()?;

        let mut need_size_update = false;
        let cur_size = self.size.load(Ordering::Relaxed);
        let new_size = cur_size.max(end_byte as u32);
        if new_size > cur_size {
            self.size.store(new_size, Ordering::Relaxed);
            need_size_update = true;
        }

        if need_size_update || current_first_clus != self.first_clus.load(Ordering::Relaxed) {
            update_entry_cluster_and_size(
                &self.sb, self.parent_clus, &self.entry_name,
                None, Some(new_size),
            )?;
        }

        Ok(buf.len())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        // Always re-read for ".." to avoid stale cache after rename updates
        // the .. entry on disk.
        let slots = if name == ".." {
            read_dir_slots(&self.sb, self.first_clus.load(Ordering::Relaxed))?
        } else {
            self.get_dir_slots()?
        };
        for slot in &slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                let fc = first_clus_from_entry(&slot.sfn_entry);
                let actual_clus = if name == ".." && fc == 0 { self.sb.bpb.root_clus } else { fc };
                let sz = file_size_from_entry(&slot.sfn_entry);
                let attr = slot.sfn_entry[0x0B];
                let ft = if attr & ATTR_DIRECTORY != 0 { FileType::Directory } else { FileType::Regular };
                return Ok(Arc::new(Fat32Inode {
                    sb: self.sb.clone(),
                    first_clus: AtomicU32::new(actual_clus),
                    size: AtomicU32::new(sz),
                    file_type: ft,
                    ino: self.sb.next_ino.fetch_add(1, Ordering::Relaxed),
                    parent_clus: self.first_clus.load(Ordering::Relaxed),
                    entry_name: String::from(name),
                    unlinked: AtomicBool::new(false),
                    dir_cache: Mutex::new(None),
                    dir_generation: AtomicU64::new(0),
                    dir_lock: Mutex::new(()),
                    write_lock: Mutex::new(()),
                }) as Arc<dyn InodeOps>);
            }
        }
        Err(VfsError::NotFound)
    }

    fn create(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] create enter\n"));
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        let _lock = self.dir_lock.lock();
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] create get_dir_slots\n"));
        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] create got slots\n"));
        if slots.iter().any(|s| decode_entry_name(s).eq_ignore_ascii_case(name)) {
            return Err(VfsError::AlreadyExists);
        }
        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots.iter()
            .map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                buf
            })
            .collect();
        let sfn = sfn_from_name(name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(name) {
            new_entries.extend(encode_vfat_entries(name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = ATTR_ARCHIVE;
        set_first_clus_in_entry(&mut sfn_entry, 0);
        set_file_size_in_entry(&mut sfn_entry, 0);
        set_timestamps(&mut sfn_entry);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] create write_dir_entries\n"));
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] create done\n"));
        self.invalidate_dir_cache();
        drop(_lock);

        let ino = self.sb.next_ino.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: AtomicU32::new(0),
            size: AtomicU32::new(0),
            file_type: FileType::Regular,
            ino,
            parent_clus: self.first_clus.load(Ordering::Relaxed),
            entry_name: String::from(name),
            unlinked: AtomicBool::new(false),
            dir_cache: Mutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        }) as Arc<dyn InodeOps>)
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] unlink enter\n"));
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        if name == "." || name == ".." { return Err(VfsError::InvalidInput); }
        let _lock = self.dir_lock.lock();

        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] unlink got slots\n"));
        let mut target_clus = 0u32;
        let mut found = false;
        for slot in &slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                if slot.sfn_entry[0x0B] & ATTR_DIRECTORY != 0 {
                    return Err(VfsError::IsADirectory);
                }
                target_clus = first_clus_from_entry(&slot.sfn_entry);
                found = true;
                break;
            }
        }
        if !found { return Err(VfsError::NotFound); }

        // Remove the directory entry only — cluster cleanup is deferred to
        // Fat32Inode::Drop so that open file handles remain valid until the
        // last reference is released (Unix semantics).
        let parent = self.first_clus.load(Ordering::Relaxed);
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] unlink remove_dir_entries\n"));
        remove_dir_entries(&self.sb, parent, name)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] unlink flush\n"));
        self.sb.flush_fat_cache()?;
        self.invalidate_dir_cache();
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] unlink done\n"));
        Ok(())
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        let _lock = self.dir_lock.lock();
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir enter\n"));
        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir got slots\n"));
        if slots.iter().any(|s| decode_entry_name(s).eq_ignore_ascii_case(name)) {
            return Err(VfsError::AlreadyExists);
        }

        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir alloc_cluster\n"));
        let new_clus = self.sb.alloc_cluster()?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir zero_cluster\n"));
        zero_cluster(&self.sb, new_clus)?;

        let empty_set = HashSet::new();
        let dot_sfn = sfn_from_name(".", &empty_set).unwrap();
        let dotdot_sfn = sfn_from_name("..", &empty_set).unwrap();

        let mut dot_entry = [0u8; DIR_ENTRY_SIZE];
        dot_entry[..MAX_SFN_LEN].copy_from_slice(&dot_sfn);
        dot_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut dot_entry, new_clus);
        set_timestamps(&mut dot_entry);

        let mut dotdot_entry = [0u8; DIR_ENTRY_SIZE];
        dotdot_entry[..MAX_SFN_LEN].copy_from_slice(&dotdot_sfn);
        dotdot_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut dotdot_entry, self.first_clus.load(Ordering::Relaxed));
        set_timestamps(&mut dotdot_entry);

        let clus_bytes = self.sb.bpb.byts_per_clus as usize;
        let mut clus_buf = alloc::vec![0u8; clus_bytes];
        clus_buf[..DIR_ENTRY_SIZE].copy_from_slice(&dot_entry);
        clus_buf[DIR_ENTRY_SIZE..2 * DIR_ENTRY_SIZE].copy_from_slice(&dotdot_entry);
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir write_cluster\n"));
        write_cluster(&self.sb, new_clus, &clus_buf)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir wrote dot/dotdot\n"));

        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots.iter()
            .map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                buf
            })
            .collect();
        let sfn = sfn_from_name(name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(name) {
            new_entries.extend(encode_vfat_entries(name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut sfn_entry, new_clus);
        set_timestamps(&mut sfn_entry);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir write_dir_entries\n"));
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        fat_trace!(crate::drivers::serial::SerialPort::puts("[DBG:fat32] mkdir done\n"));
        self.invalidate_dir_cache();
        drop(_lock);

        let ino = self.sb.next_ino.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: AtomicU32::new(new_clus),
            size: AtomicU32::new(0),
            file_type: FileType::Directory,
            ino,
            parent_clus: self.first_clus.load(Ordering::Relaxed),
            entry_name: String::from(name),
            unlinked: AtomicBool::new(false),
            dir_cache: Mutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        }) as Arc<dyn InodeOps>)
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        if name == "." || name == ".." { return Err(VfsError::InvalidInput); }
        let _lock = self.dir_lock.lock();

        let slots = self.get_dir_slots()?;
        let mut target_clus = 0u32;
        let mut found = false;
        for slot in &slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(name) {
                let attr = slot.sfn_entry[0x0B];
                if attr & ATTR_DIRECTORY == 0 { return Err(VfsError::NotADirectory); }
                target_clus = first_clus_from_entry(&slot.sfn_entry);
                found = true;
                break;
            }
        }
        if !found { return Err(VfsError::NotFound); }

        let child_slots = read_dir_slots(&self.sb, target_clus)?;
        if child_slots.len() > 2 { return Err(VfsError::NotEmpty); }

        // Remove the directory entry only — cluster cleanup is deferred to
        // Fat32Inode::Drop.
        let parent = self.first_clus.load(Ordering::Relaxed);
        remove_dir_entries(&self.sb, parent, name)?;
        self.sb.flush_fat_cache()?;
        self.invalidate_dir_cache();
        Ok(())
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        fat_trace!(crate::drivers::serial::dump_puts("[DBG:fat32] readdir enter\n"));
        let slots = self.get_dir_slots()?;
        fat_trace!(crate::drivers::serial::dump_puts("[DBG:fat32] readdir got slots\n"));
        let mut entries = Vec::with_capacity(slots.len());
        for slot in &slots {
            let name = decode_entry_name(slot);
            if name.is_empty() || name == "." || name == ".." { continue; }
            let fc = first_clus_from_entry(&slot.sfn_entry);
            let attr = slot.sfn_entry[0x0B];
            let ft = if attr & ATTR_DIRECTORY != 0 { FileType::Directory } else { FileType::Regular };
            entries.push(DirEntry { ino: fc as u64, name, file_type: ft });
        }
        Ok(entries)
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: self.ino,
            size: if self.file_type == FileType::Directory { 0 } else { self.size.load(Ordering::Relaxed) as u64 },
            file_type: self.file_type,
            mtime: 0,
        })
    }

    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        let _lock = self.dir_lock.lock();

        let slots = self.get_dir_slots()?;
        let mut target = None;
        for slot in &slots {
            if decode_entry_name(slot).eq_ignore_ascii_case(old_name) {
                target = Some(slot);
                break;
            }
        }
        let target = target.ok_or(VfsError::NotFound)?;
        let fc = first_clus_from_entry(&target.sfn_entry);
        let sz = file_size_from_entry(&target.sfn_entry);
        let attr = target.sfn_entry[0x0B];
        let is_dir = (attr & ATTR_DIRECTORY) != 0;

        // Inline removal of existing new_name (cannot call self.unlink while holding dir_lock)
        if let Some(existing) = slots.iter().find(|s| decode_entry_name(s).eq_ignore_ascii_case(new_name)) {
            let existing_clus = first_clus_from_entry(&existing.sfn_entry);
            remove_dir_entries(&self.sb, self.first_clus.load(Ordering::Relaxed), new_name)?;
            self.sb.flush_fat_cache()?;
            if existing_clus >= 2 && existing_clus < EOC_MARKER {
                self.sb.free_chain(existing_clus)?;
            }
            self.sb.flush_fat_cache()?;
            self.invalidate_dir_cache();
        }

        let target_sfn: [u8; MAX_SFN_LEN] = {
            let mut buf = [b' '; MAX_SFN_LEN];
            buf.copy_from_slice(&target.sfn_entry[..MAX_SFN_LEN]);
            buf
        };
        let existing_sfns: HashSet<[u8; MAX_SFN_LEN]> = slots.iter()
            .filter_map(|s| {
                let mut buf = [b' '; MAX_SFN_LEN];
                buf.copy_from_slice(&s.sfn_entry[..MAX_SFN_LEN]);
                if buf == target_sfn { None } else { Some(buf) }
            })
            .collect();
        let sfn = sfn_from_name(new_name, &existing_sfns).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if needs_vfat(new_name) {
            new_entries.extend(encode_vfat_entries(new_name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = attr;
        set_first_clus_in_entry(&mut sfn_entry, fc);
        set_file_size_in_entry(&mut sfn_entry, sz);
        set_timestamps(&mut sfn_entry);
        new_entries.push(sfn_entry);

        let parent = self.first_clus.load(Ordering::Relaxed);
        write_dir_entries(&self.sb, &parent, &new_entries)?;
        remove_dir_entries(&self.sb, parent, old_name)?;
        self.invalidate_dir_cache();

        if is_dir && fc >= 2 && fc < EOC_MARKER && fc != self.sb.bpb.root_clus {
            self.sb.flush_fat_cache()?;
            let clus_bytes = self.sb.bpb.byts_per_clus as usize;
            let mut buf = alloc::vec![0u8; clus_bytes];
            read_cluster(&self.sb, fc, &mut buf)?;
            let dotdot_off = DIR_ENTRY_SIZE;
            set_first_clus_in_entry(&mut buf[dotdot_off..dotdot_off + DIR_ENTRY_SIZE].try_into().unwrap(), parent);
            write_cluster(&self.sb, fc, &buf)?;
        }

        Ok(())
    }

    fn truncate(&self, len: u64) -> Result<(), VfsError> {
        if self.file_type != FileType::Regular { return Err(VfsError::IsADirectory); }
        if len > u32::MAX as u64 { return Err(VfsError::FileTooLarge); }
        let _write_lock = self.write_lock.lock();

        let new_size = len as u32;
        let clus_size = self.sb.bpb.byts_per_clus;
        let needed = if new_size == 0 { 0 } else { ((new_size as u64 - 1) / clus_size as u64 + 1) as u32 };
        let current_first = self.first_clus.load(Ordering::Relaxed);
        let have = self.sb.chain_len(current_first)?;

        if new_size == 0 && current_first != 0 {
            // Remove dir entry reference first, then free clusters
            update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name,
                                           Some(0), Some(0))?;
            self.sb.flush_fat_cache()?;
            self.sb.free_chain(current_first)?;
            self.sb.flush_fat_cache()?;
            self.first_clus.store(0, Ordering::Relaxed);
            self.size.store(0, Ordering::Relaxed);
        } else if needed < have && current_first != 0 {
            self.sb.truncate_chain(current_first, needed)?;
            self.sb.flush_fat_cache()?;
            self.size.store(new_size, Ordering::Relaxed);
            update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name,
                                           None, Some(new_size))?;
        } else if needed > have {
            if current_first == 0 {
                let new_clus = self.sb.alloc_cluster()?;
                zero_cluster(&self.sb, new_clus)?;
                if needed > 1 {
                    self.sb.extend_chain(new_clus, needed - 1)?;
                }
                self.sb.flush_fat_cache()?;
                self.first_clus.store(new_clus, Ordering::Relaxed);
                self.size.store(new_size, Ordering::Relaxed);
                update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name,
                                               Some(new_clus), Some(new_size))?;
                return Ok(());
            }
            self.sb.extend_chain(current_first, needed - have)?;
            self.sb.flush_fat_cache()?;
            self.size.store(new_size, Ordering::Relaxed);
            update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name,
                                           None, Some(new_size))?;
        } else {
            self.size.store(new_size, Ordering::Relaxed);
        }

        Ok(())
    }

    fn file_type(&self) -> FileType { self.file_type }
    fn ino(&self) -> u64 { self.ino }
    fn size(&self) -> u64 {
        if self.file_type == FileType::Directory { 0 } else { self.size.load(Ordering::Relaxed) as u64 }
    }

    fn on_unlink(&self) {
        self.unlinked.store(true, Ordering::Relaxed);
    }
}

// ── Fat32FileSystem (implements FileSystem) ──────────────────────────────────

pub struct Fat32FileSystem;

impl FileSystem for Fat32FileSystem {
    fn name(&self) -> &str { "fat32" }

    fn mount(&self, device: Option<Arc<dyn BlockDevice>>)
             -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError>
    {
        let dev = device.ok_or(VfsError::InvalidDevice)?;
        let cached = CachedDevice::new(dev.clone());
        let bpb = parse_bpb(&*cached)?;

        // Check if the volume was cleanly unmounted
        {
            let mut sector = [0u8; SECTOR_SIZE];
            read_sectors(&*cached, 0, 1, &mut sector)?;
            if sector[0x41] & 1 != 0 {
                log::warn!("FAT32: volume was not cleanly unmounted (dirty bit set)");
            }
        }

        let sb = Arc::new(Fat32SuperBlock {
            device: cached,
            bpb: bpb.clone(),
            fat_cache: Mutex::new(FatCache::new()),
            next_ino: AtomicU64::new(2),
            next_alloc_hint: Mutex::new(2),
            free_clus_count: AtomicU32::new(0),
            volume_dirty: AtomicBool::new(false),
        });

        // Read FSInfo next_alloc_hint to seed the allocator (best-effort)
        if bpb.fsinfo_is_valid() {
            let mut sector = [0u8; SECTOR_SIZE];
            if read_sectors(&*sb.device, bpb.fsinfo_sec as u64, 1, &mut sector).is_ok() {
                if sector[0..4] == FSINFO_LEAD_SIG.to_le_bytes()
                    && sector[484..488] == FSINFO_STRUCT_SIG.to_le_bytes()
                {
                    let hint = u32::from_le_bytes([sector[492], sector[493], sector[494], sector[495]]);
                    if hint >= 2 && hint < 2 + bpb.total_clus {
                        *sb.next_alloc_hint.lock() = hint;
                    }
                }
            }
        }

        // Scan free clusters on mount for accurate statfs + FSInfo
        let free = sb.scan_free_clusters()?;
        sb.free_clus_count.store(free, Ordering::Relaxed);

        let root_clus = sb.bpb.root_clus;
        let root_ops = Arc::new(Fat32Inode {
            sb: sb.clone(),
            first_clus: AtomicU32::new(root_clus),
            size: AtomicU32::new(0),
            file_type: FileType::Directory,
            ino: 1,
            parent_clus: root_clus,
            entry_name: String::new(),
            unlinked: AtomicBool::new(false),
            dir_cache: Mutex::new(None),
            dir_generation: AtomicU64::new(0),
            dir_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
        }) as Arc<dyn InodeOps>;

        let root_inode = Arc::new(crate::filesystems::vfs::inode::Inode::new(root_ops.clone()));
        let super_ops = sb.clone() as Arc<dyn SuperOps>;
        let sb_vfs = Arc::new(SuperBlock::new(super_ops, root_inode));
        Ok((sb_vfs, root_ops))
    }
}
