use core::sync::atomic::{AtomicU64, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};
use spin::Mutex;

use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoRequest};
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::superblock::{SuperBlock, SuperOps, StatFs};
use crate::filesystems::vfs::types::{DirEntry, FileType, Stat};
use super::FileSystem;

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

// ── BPB (BIOS Parameter Block) ──────────────────────────────────────────────

#[derive(Clone)]
struct Bpb {
    bytes_per_sec: u16,
    sec_per_clus: u8,
    rsvd_sec_cnt: u16,
    num_fats: u8,
    fat_sz32: u32,
    root_clus: u32,
    byts_per_clus: u32,
    first_data_sec: u64,
    total_clus: u32,
}

fn parse_bpb(device: &dyn BlockDevice) -> Result<Bpb, VfsError> {
    let mut sector = [0u8; SECTOR_SIZE];
    read_sectors(device, 0, 1, &mut sector)?;

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(VfsError::InvalidDevice);
    }
    if sector[0x52] != 0x61 || sector[0x53] != 0x41 ||
       sector[0x54] != 0x72 || sector[0x55] != 0x72
    {
        return Err(VfsError::InvalidDevice);
    }

    let bytes_per_sec = u16::from_le_bytes([sector[0x0B], sector[0x0C]]);
    let sec_per_clus = sector[0x0D];
    let rsvd_sec_cnt = u16::from_le_bytes([sector[0x0E], sector[0x0F]]);
    let num_fats = sector[0x10];
    let fat_sz32 = u32::from_le_bytes([sector[0x24], sector[0x25], sector[0x26], sector[0x27]]);
    let root_clus = u32::from_le_bytes([sector[0x2C], sector[0x2D], sector[0x2E], sector[0x2F]]);
    let first_data_sec = rsvd_sec_cnt as u64 + (num_fats as u64) * fat_sz32 as u64;

    let total_sectors = {
        let sz16 = u16::from_le_bytes([sector[0x13], sector[0x14]]);
        if sz16 != 0 { sz16 as u64 } else {
            u32::from_le_bytes([sector[0x20], sector[0x21], sector[0x22], sector[0x23]]) as u64
        }
    };
    let total_data_sectors = total_sectors - first_data_sec;
    let total_clus = (total_data_sectors / sec_per_clus as u64) as u32;
    let byts_per_clus = (bytes_per_sec as u32) * (sec_per_clus as u32);

    Ok(Bpb {
        bytes_per_sec, sec_per_clus, rsvd_sec_cnt, num_fats,
        fat_sz32, root_clus,
        byts_per_clus, first_data_sec, total_clus,
    })
}

impl Bpb {
    fn cluster_to_lba(&self, cluster: u32) -> u64 {
        self.first_data_sec + ((cluster - 2) as u64) * (self.sec_per_clus as u64)
    }

    fn fat_sector_lba(&self, fat_num: u8, sector_idx: u32) -> u64 {
        self.rsvd_sec_cnt as u64
            + (fat_num as u64) * self.fat_sz32 as u64
            + sector_idx as u64
    }

    fn fat_entry_position(&self, cluster: u32) -> (u32, u32) {
        let byte_off = cluster as u32 * 4;
        let sector_idx = byte_off / self.bytes_per_sec as u32;
        let offset = byte_off % self.bytes_per_sec as u32;
        (sector_idx, offset)
    }
}

// ── FAT cache ───────────────────────────────────────────────────────────────

struct FatCache {
    sectors: HashMap<u64, [u8; SECTOR_SIZE]>,
    dirty: HashSet<u64>,
}

impl FatCache {
    fn new() -> Self {
        FatCache { sectors: HashMap::new(), dirty: HashSet::new() }
    }

    fn get_or_read(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&[u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.sectors.insert(lba, buf);
        }
        Ok(self.sectors.get(&lba).unwrap())
    }

    fn get_or_read_mut(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&mut [u8; SECTOR_SIZE], VfsError> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; SECTOR_SIZE];
            read_sectors(device, lba, 1, &mut buf)?;
            self.sectors.insert(lba, buf);
        }
        self.dirty.insert(lba);
        Ok(self.sectors.get_mut(&lba).unwrap())
    }

    fn flush(&mut self, device: &dyn BlockDevice, bpb: &Bpb) -> Result<(), VfsError> {
        for &lba in self.dirty.iter() {
            let data = self.sectors.get(&lba).unwrap();
            write_sectors(device, lba, 1, data)?;
            for fat_num in 1..bpb.num_fats {
                let local_idx = (lba - bpb.rsvd_sec_cnt as u64) % bpb.fat_sz32 as u64;
                write_sectors(device, bpb.fat_sector_lba(fat_num, local_idx as u32), 1, data)?;
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
}

impl Fat32SuperBlock {
    fn read_fat_entry(&self, cluster: u32) -> Result<u32, VfsError> {
        let (sector_idx, offset) = self.bpb.fat_entry_position(cluster);
        let lba = self.bpb.fat_sector_lba(0, sector_idx);
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
        let lba = self.bpb.fat_sector_lba(0, sector_idx);
        let mut cache = self.fat_cache.lock();
        let sector = cache.get_or_read_mut(&*self.device, lba)?;
        let bytes = (value & 0x0FFFFFFF).to_le_bytes();
        sector[offset as usize..offset as usize + 4].copy_from_slice(&bytes);
        Ok(())
    }

    fn alloc_cluster(&self) -> Result<u32, VfsError> {
        let mut hint = self.next_alloc_hint.lock();
        let n = self.bpb.total_clus;
        for i in 0..n {
            let clus = 2 + ((*hint - 2 + i) % n);
            if self.read_fat_entry(clus)? == FREE_CLUSTER {
                self.write_fat_entry(clus, EOC_MARKER)?;
                *hint = clus + 1;
                return Ok(clus);
            }
        }
        Err(VfsError::NoSpace)
    }

    fn free_chain(&self, mut cluster: u32) -> Result<(), VfsError> {
        while cluster >= 2 && cluster < EOC_MARKER {
            let next = self.read_fat_entry(cluster)?;
            self.write_fat_entry(cluster, FREE_CLUSTER)?;
            cluster = next;
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
        }
        Ok(n)
    }

    fn extend_chain(&self, start: u32, additional: u32) -> Result<(), VfsError> {
        let mut tail = start;
        loop {
            let next = self.read_fat_entry(tail)?;
            if next >= EOC_MARKER { break; }
            tail = next;
        }
        for _ in 0..additional {
            let new = self.alloc_cluster()?;
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

    fn sync_all(&self) -> Result<(), VfsError> {
        let mut cache = self.fat_cache.lock();
        cache.flush(&*self.device, &self.bpb)
    }
}

impl SuperOps for Fat32SuperBlock {
    fn statfs(&self) -> Result<StatFs, VfsError> {
        Ok(StatFs {
            block_size: self.bpb.byts_per_clus,
            total_blocks: self.bpb.total_clus as u64,
            free_blocks: 0,
        })
    }
    fn sync_fs(&self) -> Result<(), VfsError> {
        self.sync_all()
    }
}

// ── Sector/cluster I/O helpers ──────────────────────────────────────────────

fn read_sectors(device: &dyn BlockDevice, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), VfsError> {
    let req = IoRequest { lba, count, buffer: IoBuffer::Buf(buf), is_write: false };
    let c = device.submit(&[req]).map_err(|_| VfsError::IOError)?;
    if !c.all_ok() { return Err(VfsError::IOError); }
    Ok(())
}

fn write_sectors(device: &dyn BlockDevice, lba: u64, count: u32, buf: &[u8]) -> Result<(), VfsError> {
    let ptr = buf.as_ptr() as *mut u8;
    let mut_buf = unsafe { &mut *core::ptr::slice_from_raw_parts_mut(ptr, buf.len()) };
    let req = IoRequest { lba, count, buffer: IoBuffer::Buf(mut_buf), is_write: true };
    let c = device.submit(&[req]).map_err(|_| VfsError::IOError)?;
    if !c.all_ok() { return Err(VfsError::IOError); }
    Ok(())
}

fn read_cluster(sb: &Fat32SuperBlock, cluster: u32, buf: &mut [u8]) -> Result<(), VfsError> {
    let lba = sb.bpb.cluster_to_lba(cluster);
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

// ── Name encoding/decoding ──────────────────────────────────────────────────

fn decode_sfn(sfn: &[u8; MAX_SFN_LEN]) -> String {
    let mut name = String::new();
    let stem_end = sfn[..8].iter().rposition(|&b| b != b' ').map(|p| p + 1).unwrap_or(0);
    name.push_str(core::str::from_utf8(&sfn[..stem_end]).unwrap_or(""));
    let ext_start = sfn[8..11].iter().position(|&b| b == b' ').unwrap_or(3);
    if ext_start > 0 {
        name.push('.');
        name.push_str(core::str::from_utf8(&sfn[8..8 + ext_start]).unwrap_or(""));
    }
    name
}

fn sfn_from_name(name: &str) -> Option<[u8; MAX_SFN_LEN]> {
    if name.is_empty() { return None; }
    let mut sfn = [b' '; MAX_SFN_LEN];
    let (stem, ext) = if let Some(dot) = name.rfind('.') {
        if dot == 0 { ("", &name[1..]) } else { (&name[..dot], &name[dot + 1..]) }
    } else {
        (name, "")
    };
    for (i, &b) in stem.as_bytes().iter().enumerate() {
        if i >= 8 { break; }
        sfn[i] = b.to_ascii_uppercase();
    }
    for (i, &b) in ext.as_bytes().iter().enumerate() {
        if i >= 3 { break; }
        sfn[8 + i] = b.to_ascii_uppercase();
    }
    Some(sfn)
}

fn vfat_checksum(sfn: &[u8; MAX_SFN_LEN]) -> u8 {
    let mut sum: u8 = 0;
    for i in 0..MAX_SFN_LEN {
        sum = ((sum >> 1) | (sum << 7)).wrapping_add(sfn[i]);
    }
    sum
}

fn decode_vfat_name(entries: &[[u8; DIR_ENTRY_SIZE]]) -> String {
    let mut utf16_buf: Vec<u16> = Vec::new();
    for entry in entries.iter().rev() {
        if entry[0] & 0x1F == 0 { continue; }
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

struct DirEntrySlot {
    vfat_entries: Vec<[u8; DIR_ENTRY_SIZE]>,
    sfn_entry: [u8; DIR_ENTRY_SIZE],
}

fn read_dir_slots(sb: &Fat32SuperBlock, dir_clus: u32) -> Result<Vec<DirEntrySlot>, VfsError> {
    let mut slots: Vec<DirEntrySlot> = Vec::new();
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut buf = alloc::vec![0u8; clus_bytes];
    let mut cluster = dir_clus;
    let mut vfat_chain: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();

    loop {
        read_cluster(sb, cluster, &mut buf)?;
        for i in 0..entries_per_clus {
            let off = i * DIR_ENTRY_SIZE;
            let entry: &[u8; DIR_ENTRY_SIZE] = &buf[off..off + DIR_ENTRY_SIZE].try_into().unwrap();
            if entry[0] == DIR_END { break; }
            if entry[0] == DIR_DELETED { vfat_chain.clear(); continue; }
            let attr = entry[0x0B];
            if attr == ATTR_LONG_NAME { vfat_chain.push(*entry); continue; }
            if attr & ATTR_VOLUME_ID != 0 { vfat_chain.clear(); continue; }
            slots.push(DirEntrySlot { vfat_entries: core::mem::take(&mut vfat_chain), sfn_entry: *entry });
        }
        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER { break; }
        cluster = next;
    }
    Ok(slots)
}

fn decode_entry_name(slot: &DirEntrySlot) -> String {
    if !slot.vfat_entries.is_empty() {
        decode_vfat_name(&slot.vfat_entries)
    } else {
        decode_sfn(&slot.sfn_entry[..MAX_SFN_LEN].try_into().unwrap_or([b' '; MAX_SFN_LEN]))
    }
}

// ── Directory writing / updating helpers ────────────────────────────────────

/// Write a chain of directory entries (VFAT + SFN) into a directory cluster chain.
/// Extends the chain if needed. Updates `dir_clus` if the starting cluster changes.
fn write_dir_entries(sb: &Fat32SuperBlock, dir_clus: &mut u32,
                     entries: &[[u8; DIR_ENTRY_SIZE]]) -> Result<(), VfsError>
{
    if entries.is_empty() { return Ok(()); }
    let total = entries.len();
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut placed = 0usize;
    let mut cluster = *dir_clus;
    let mut buf = alloc::vec![0u8; clus_bytes];

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

/// Find the sector/cluster containing the named entry and apply a mutation
/// closure to the SFN entry.
fn find_and_update_entry<F>(sb: &Fat32SuperBlock, dir_clus: u32, name: &str,
                             mut f: F) -> Result<(), VfsError>
where
    F: FnMut(&mut [u8; DIR_ENTRY_SIZE]),
{
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut buf = alloc::vec![0u8; clus_bytes];

    let mut cluster = dir_clus;
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

            // Find chain start
            let mut chain_start = i;
            while chain_start > 0 && buf[(chain_start - 1) * DIR_ENTRY_SIZE + 0x0B] == ATTR_LONG_NAME {
                chain_start -= 1;
            }

            // Collect VFAT entries
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

            if entry_name == name {
                let mut sfn_entry: [u8; DIR_ENTRY_SIZE] = buf[off..off + DIR_ENTRY_SIZE].try_into().unwrap();
                f(&mut sfn_entry);
                buf[off..off + DIR_ENTRY_SIZE].copy_from_slice(&sfn_entry);
                write_cluster(sb, cluster, &buf)?;
                return Ok(());
            }
            i += 1;
        }

        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER { break; }
        cluster = next;
    }
    Err(VfsError::NotFound)
}

/// Update the first_clus and/or size fields of a directory entry on disk.
fn update_entry_cluster_and_size(sb: &Fat32SuperBlock, dir_clus: u32,
                                  name: &str, new_clus: Option<u32>,
                                  new_size: Option<u32>) -> Result<(), VfsError>
{
    find_and_update_entry(sb, dir_clus, name, |entry| {
        if let Some(c) = new_clus { set_first_clus_in_entry(entry, c); }
        if let Some(s) = new_size { set_file_size_in_entry(entry, s); }
    })
}

/// Remove a named entry's directory entries (VFAT + SFN) by marking them deleted.
fn remove_dir_entries(sb: &Fat32SuperBlock, dir_clus: u32, name: &str) -> Result<(), VfsError> {
    let clus_bytes = sb.bpb.byts_per_clus as usize;
    let entries_per_clus = clus_bytes / DIR_ENTRY_SIZE;
    let mut buf = alloc::vec![0u8; clus_bytes];

    let mut cluster = dir_clus;
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

            if entry_name == name {
                for j in chain_start..=i {
                    buf[j * DIR_ENTRY_SIZE] = DIR_DELETED;
                }
                write_cluster(sb, cluster, &buf)?;
                return Ok(());
            }
            i += 1;
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
    first_clus: u32,
    size: u32,
    file_type: FileType,
    ino: u64,
    parent_clus: u32,
    entry_name: String,
}

impl Fat32Inode {
    fn sync_clus_and_size(&self) -> Result<(), VfsError> {
        update_entry_cluster_and_size(
            &self.sb, self.parent_clus, &self.entry_name,
            Some(self.first_clus), Some(self.size),
        )
    }
}

impl InodeOps for Fat32Inode {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.file_type != FileType::Regular { return Err(VfsError::IsADirectory); }
        if offset >= self.size as u64 || buf.is_empty() || self.first_clus == 0 { return Ok(0); }

        let clus_size = self.sb.bpb.byts_per_clus as u64;
        let total = (buf.len() as u64).min(self.size as u64 - offset) as usize;
        let start_idx = (offset / clus_size) as u32;
        let mut current = if start_idx == 0 {
            self.first_clus
        } else {
            self.sb.chain_cluster_at(self.first_clus, start_idx).unwrap_or(self.first_clus)
        };

        if current >= EOC_MARKER || current < 2 { return Ok(0); }

        let clus_bytes = clus_size as usize;
        let mut cluster_buf = alloc::vec![0u8; clus_bytes];
        let mut done = 0usize;
        let mut clus_off = (offset % clus_size) as usize;

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
        }
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        if self.file_type != FileType::Regular { return Err(VfsError::IsADirectory); }
        if buf.is_empty() { return Ok(0); }

        let clus_size = self.sb.bpb.byts_per_clus as u64;
        let end_byte = offset + buf.len() as u64;
        let needed_clus = if end_byte == 0 { 0 } else { ((end_byte - 1) / clus_size + 1) as u32 };

        // Ensure we have enough clusters
        let mut current_first_clus = self.first_clus;
        if needed_clus > 0 {
            let have = self.sb.chain_len(current_first_clus)?;
            if have < needed_clus {
                if current_first_clus == 0 {
                    current_first_clus = self.sb.alloc_cluster()?;
                    zero_cluster(&self.sb, current_first_clus)?;
                    if needed_clus > 1 {
                        self.sb.extend_chain(current_first_clus, needed_clus - 1)?;
                    }
                    // Update the on-disk directory entry with the new first_clus
                    self.sync_clus_and_size()?;
                } else {
                    self.sb.extend_chain(current_first_clus, needed_clus - have)?;
                }
            }
        }

        // Walk to the starting cluster
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
        }

        // Update size on disk
        let new_size = self.size.max(end_byte as u32);
        if new_size > self.size {
            update_entry_cluster_and_size(
                &self.sb, self.parent_clus, &self.entry_name,
                None, Some(new_size),
            )?;
        }

        Ok(buf.len())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        let slots = read_dir_slots(&self.sb, self.first_clus)?;
        for slot in &slots {
            if decode_entry_name(slot) == name {
                let fc = first_clus_from_entry(&slot.sfn_entry);
                let actual_clus = if name == ".." && fc == 0 { self.sb.bpb.root_clus } else { fc };
                let sz = file_size_from_entry(&slot.sfn_entry);
                let attr = slot.sfn_entry[0x0B];
                let ft = if attr & ATTR_DIRECTORY != 0 { FileType::Directory } else { FileType::Regular };
                return Ok(Arc::new(Fat32Inode {
                    sb: self.sb.clone(),
                    first_clus: actual_clus,
                    size: sz,
                    file_type: ft,
                    ino: self.sb.next_ino.fetch_add(1, Ordering::Relaxed),
                    parent_clus: self.first_clus,
                    entry_name: String::from(name),
                }) as Arc<dyn InodeOps>);
            }
        }
        Err(VfsError::NotFound)
    }

    fn create(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        // Check for existing entry
        if read_dir_slots(&self.sb, self.first_clus)?.iter().any(|s| decode_entry_name(s) == name) {
            return Err(VfsError::AlreadyExists);
        }

        let sfn = sfn_from_name(name).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if name.len() > 12 || name.bytes().any(|b| b > 127 || b == b' ') {
            new_entries.extend(encode_vfat_entries(name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = ATTR_ARCHIVE;
        set_first_clus_in_entry(&mut sfn_entry, 0);
        set_file_size_in_entry(&mut sfn_entry, 0);
        new_entries.push(sfn_entry);

        let mut parent = self.first_clus;
        write_dir_entries(&self.sb, &mut parent, &new_entries)?;

        let ino = self.sb.next_ino.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: 0,
            size: 0,
            file_type: FileType::Regular,
            ino,
            parent_clus: self.first_clus,
            entry_name: String::from(name),
        }) as Arc<dyn InodeOps>)
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }

        // Find the target's first cluster to free its chain
        let slots = read_dir_slots(&self.sb, self.first_clus)?;
        let mut target_clus = 0u32;
        let mut found = false;
        for slot in &slots {
            if decode_entry_name(slot) == name {
                target_clus = first_clus_from_entry(&slot.sfn_entry);
                found = true;
                break;
            }
        }
        if !found { return Err(VfsError::NotFound); }

        if target_clus >= 2 && target_clus < EOC_MARKER {
            self.sb.free_chain(target_clus)?;
        }
        remove_dir_entries(&self.sb, self.first_clus, name)
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        if read_dir_slots(&self.sb, self.first_clus)?.iter().any(|s| decode_entry_name(s) == name) {
            return Err(VfsError::AlreadyExists);
        }

        let new_clus = self.sb.alloc_cluster()?;
        zero_cluster(&self.sb, new_clus)?;

        let dot_sfn = sfn_from_name(".").unwrap();
        let dotdot_sfn = sfn_from_name("..").unwrap();

        let mut dot_entry = [0u8; DIR_ENTRY_SIZE];
        dot_entry[..MAX_SFN_LEN].copy_from_slice(&dot_sfn);
        dot_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut dot_entry, new_clus);

        let mut dotdot_entry = [0u8; DIR_ENTRY_SIZE];
        dotdot_entry[..MAX_SFN_LEN].copy_from_slice(&dotdot_sfn);
        dotdot_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut dotdot_entry, self.first_clus);

        let clus_bytes = self.sb.bpb.byts_per_clus as usize;
        let mut clus_buf = alloc::vec![0u8; clus_bytes];
        clus_buf[..DIR_ENTRY_SIZE].copy_from_slice(&dot_entry);
        clus_buf[DIR_ENTRY_SIZE..2 * DIR_ENTRY_SIZE].copy_from_slice(&dotdot_entry);
        write_cluster(&self.sb, new_clus, &clus_buf)?;

        // Create directory entry in parent
        let sfn = sfn_from_name(name).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if name.len() > 12 || name.bytes().any(|b| b > 127 || b == b' ') {
            new_entries.extend(encode_vfat_entries(name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = ATTR_DIRECTORY;
        set_first_clus_in_entry(&mut sfn_entry, new_clus);
        new_entries.push(sfn_entry);

        let mut parent = self.first_clus;
        write_dir_entries(&self.sb, &mut parent, &new_entries)?;

        let ino = self.sb.next_ino.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Fat32Inode {
            sb: self.sb.clone(),
            first_clus: new_clus,
            size: 0,
            file_type: FileType::Directory,
            ino,
            parent_clus: self.first_clus,
            entry_name: String::from(name),
        }) as Arc<dyn InodeOps>)
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }

        let slots = read_dir_slots(&self.sb, self.first_clus)?;
        let mut target_clus = 0u32;
        let mut found = false;
        for slot in &slots {
            if decode_entry_name(slot) == name {
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

        if target_clus >= 2 && target_clus < EOC_MARKER {
            self.sb.free_chain(target_clus)?;
        }
        remove_dir_entries(&self.sb, self.first_clus, name)
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }
        let slots = read_dir_slots(&self.sb, self.first_clus)?;
        let mut entries = Vec::with_capacity(slots.len());
        for slot in &slots {
            let name = decode_entry_name(slot);
            if name == "." || name == ".." { continue; }
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
            size: if self.file_type == FileType::Directory { 0 } else { self.size as u64 },
            file_type: self.file_type,
            mtime: 0,
        })
    }

    fn rename(&self, old_name: &str, new_name: &str) -> Result<(), VfsError> {
        if self.file_type != FileType::Directory { return Err(VfsError::NotADirectory); }

        // Read target entry data
        let slots = read_dir_slots(&self.sb, self.first_clus)?;
        let mut target = None;
        for slot in &slots {
            if decode_entry_name(slot) == old_name {
                target = Some(slot);
                break;
            }
        }
        let target = target.ok_or(VfsError::NotFound)?;
        let fc = first_clus_from_entry(&target.sfn_entry);
        let sz = file_size_from_entry(&target.sfn_entry);
        let attr = target.sfn_entry[0x0B];
        let is_dir = (attr & ATTR_DIRECTORY) != 0;

        // If new_name exists, unlink it first
        if slots.iter().any(|s| decode_entry_name(s) == new_name) {
            self.unlink(new_name)?;
        }

        // Create new entry
        let sfn = sfn_from_name(new_name).ok_or(VfsError::InvalidInput)?;
        let csum = vfat_checksum(&sfn);
        let mut new_entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
        if new_name.len() > 12 || new_name.bytes().any(|b| b > 127 || b == b' ') {
            new_entries.extend(encode_vfat_entries(new_name, csum));
        }
        let mut sfn_entry = [0u8; DIR_ENTRY_SIZE];
        sfn_entry[..MAX_SFN_LEN].copy_from_slice(&sfn);
        sfn_entry[0x0B] = attr;
        set_first_clus_in_entry(&mut sfn_entry, fc);
        set_file_size_in_entry(&mut sfn_entry, sz);
        new_entries.push(sfn_entry);

        let mut parent = self.first_clus;
        write_dir_entries(&self.sb, &mut parent, &new_entries)?;
        remove_dir_entries(&self.sb, self.first_clus, old_name)?;

        // If renaming a directory, update its ".." entry to point to us
        if is_dir && fc >= 2 && fc < EOC_MARKER {
            let clus_bytes = self.sb.bpb.byts_per_clus as usize;
            let mut buf = alloc::vec![0u8; clus_bytes];
            read_cluster(&self.sb, fc, &mut buf)?;
            // ".." is the second entry
            let dotdot_off = DIR_ENTRY_SIZE;
            set_first_clus_in_entry(&mut buf[dotdot_off..dotdot_off + DIR_ENTRY_SIZE].try_into().unwrap(), self.first_clus);
            write_cluster(&self.sb, fc, &buf)?;
        }

        Ok(())
    }

    fn truncate(&self, len: u64) -> Result<(), VfsError> {
        if self.file_type != FileType::Regular { return Err(VfsError::IsADirectory); }
        if len > u32::MAX as u64 { return Err(VfsError::InvalidInput); }

        let new_size = len as u32;
        let clus_size = self.sb.bpb.byts_per_clus;
        let needed = if new_size == 0 { 0 } else { ((new_size as u64 - 1) / clus_size as u64 + 1) as u32 };
        let have = self.sb.chain_len(self.first_clus)?;

        if new_size == 0 && self.first_clus != 0 {
            self.sb.free_chain(self.first_clus)?;
        } else if needed < have && self.first_clus != 0 {
            self.sb.truncate_chain(self.first_clus, needed)?;
        } else if needed > have {
            if self.first_clus == 0 { return Err(VfsError::NoSpace); }
            self.sb.extend_chain(self.first_clus, needed - have)?;
        }

        // Update on-disk size (first_clus unchanged for truncate unless freeing all)
        if new_size == 0 && self.first_clus != 0 {
            update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name,
                                           Some(0), Some(0))?;
        } else {
            update_entry_cluster_and_size(&self.sb, self.parent_clus, &self.entry_name,
                                           None, Some(new_size))?;
        }

        Ok(())
    }

    fn file_type(&self) -> FileType { self.file_type }
    fn ino(&self) -> u64 { self.ino }
    fn size(&self) -> u64 { if self.file_type == FileType::Directory { 0 } else { self.size as u64 } }
}

// ── Fat32FileSystem (implements FileSystem) ──────────────────────────────────

pub struct Fat32FileSystem;

impl FileSystem for Fat32FileSystem {
    fn name(&self) -> &str { "fat32" }

    fn mount(&self, device: Option<Arc<dyn BlockDevice>>)
             -> Result<(Arc<SuperBlock>, Arc<dyn InodeOps>), VfsError>
    {
        let dev = device.ok_or(VfsError::InvalidDevice)?;
        let bpb = parse_bpb(&*dev)?;

        let sb = Arc::new(Fat32SuperBlock {
            device: dev,
            bpb,
            fat_cache: Mutex::new(FatCache::new()),
            next_ino: AtomicU64::new(2),
            next_alloc_hint: Mutex::new(2),
        });

        let root_clus = sb.bpb.root_clus;
        let root_ops = Arc::new(Fat32Inode {
            sb: sb.clone(),
            first_clus: root_clus,
            size: 0,
            file_type: FileType::Directory,
            ino: 1,
            parent_clus: root_clus,
            entry_name: String::new(),
        }) as Arc<dyn InodeOps>;

        let root_inode = Arc::new(crate::filesystems::vfs::inode::Inode::new(root_ops.clone()));
        let super_ops = sb.clone() as Arc<dyn SuperOps>;
        let sb_vfs = Arc::new(SuperBlock::new(super_ops, root_inode));
        Ok((sb_vfs, root_ops))
    }
}
