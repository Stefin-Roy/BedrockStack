use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use crate::filesystems::vfs::error::VfsError;

use super::bpb::{DIR_ENTRY_SIZE, MAX_SFN_LEN};
use super::cluster::{read_cluster, write_cluster, zero_cluster};
use super::dirent::{
    ATTR_LONG_NAME, ATTR_VOLUME_ID, DIR_DELETED, DIR_END, DirEntrySlot, decode_sfn,
    decode_vfat_name, decode_volume_label, set_file_size_in_entry, set_first_clus_in_entry,
    set_timestamps,
};
use super::fat::EOC_MARKER;

macro_rules! fat_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "fat_trace")]
        $($arg)*
    };
}

use super::mount::Fat32SuperBlock;

pub(super) fn read_dir_slots(
    sb: &Fat32SuperBlock,
    dir_clus: u32,
) -> Result<Vec<DirEntrySlot>, VfsError> {
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
        fat_trace!(crate::drivers::serial::dump_puts(
            "[DBG:fat32] read_dir_slots read cluster ok\n"
        ));
        for i in 0..entries_per_clus {
            let off = i * DIR_ENTRY_SIZE;
            let entry: &[u8; DIR_ENTRY_SIZE] = buf
                .get(off..off + DIR_ENTRY_SIZE)
                .ok_or(VfsError::IOError)?
                .try_into()
                .map_err(|_| VfsError::IOError)?;
            if entry[0] == DIR_END {
                vfat_chain.clear();
                end_of_dir = true;
                break;
            }
            if entry[0] == DIR_DELETED {
                vfat_chain.clear();
                continue;
            }
            let attr = entry[0x0B];
            if attr == ATTR_LONG_NAME {
                vfat_chain.push(*entry);
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                vfat_chain.clear();
                slots.push(DirEntrySlot {
                    vfat_entries: Vec::new(),
                    sfn_entry: *entry,
                });
                continue;
            }
            slots.push(DirEntrySlot {
                vfat_entries: mem::take(&mut vfat_chain),
                sfn_entry: *entry,
            });
        }
        if end_of_dir {
            break;
        }
        let next = sb.read_fat_entry(cluster)?;
        if next >= EOC_MARKER {
            break;
        }
        cluster = next;
        _iters += 1;
        if _iters > sb.bpb.total_clus + 2 {
            return Err(VfsError::IOError);
        }
    }
    Ok(slots)
}

pub(super) fn decode_entry_name(slot: &DirEntrySlot) -> String {
    let attr = slot.sfn_entry[0x0B];
    if attr & ATTR_VOLUME_ID != 0 {
        decode_volume_label(
            &slot.sfn_entry[..MAX_SFN_LEN]
                .try_into()
                .unwrap_or([b' '; MAX_SFN_LEN]),
        )
    } else if !slot.vfat_entries.is_empty() {
        decode_vfat_name(&slot.vfat_entries)
    } else {
        decode_sfn(
            &slot.sfn_entry[..MAX_SFN_LEN]
                .try_into()
                .unwrap_or([b' '; MAX_SFN_LEN]),
        )
    }
}

pub(super) fn write_dir_entries(
    sb: &Fat32SuperBlock,
    dir_clus: &u32,
    entries: &[[u8; DIR_ENTRY_SIZE]],
) -> Result<(), VfsError> {
    fat_trace!({
        use core::fmt::Write;
        let mut port = crate::drivers::serial::SerialPort::new();
        write!(
            port,
            "[DBG:fat32] wde enter clus={} entries={}\n",
            *dir_clus,
            entries.len()
        )
        .ok();
    });
    if entries.is_empty() {
        return Ok(());
    }
    if *dir_clus < 2 {
        fat_trace!(crate::drivers::serial::SerialPort::puts(
            "[DBG:fat32] wde bad clus\n"
        ));
        return Err(VfsError::InvalidInput);
    }
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
                        if b == DIR_DELETED || b == DIR_END {
                            space += 1;
                        } else {
                            break;
                        }
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

pub(super) fn find_and_update_entry<F>(
    sb: &Fat32SuperBlock,
    dir_clus: u32,
    name: &str,
    mut f: F,
) -> Result<(), VfsError>
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
            if first == DIR_END {
                return Err(VfsError::NotFound);
            }
            if first == DIR_DELETED {
                i += 1;
                continue;
            }
            let attr = buf[off + 0x0B];
            if attr == ATTR_LONG_NAME {
                i += 1;
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                i += 1;
                continue;
            }

            let mut chain_start = i;
            while chain_start > 0
                && buf[(chain_start - 1) * DIR_ENTRY_SIZE + 0x0B] == ATTR_LONG_NAME
            {
                chain_start -= 1;
            }

            let mut vfat_buf: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
            for j in chain_start..i {
                let e: &[u8; DIR_ENTRY_SIZE] = buf
                    .get(j * DIR_ENTRY_SIZE..(j + 1) * DIR_ENTRY_SIZE)
                    .ok_or(VfsError::IOError)?
                    .try_into()
                    .map_err(|_| VfsError::IOError)?;
                vfat_buf.push(*e);
            }
            let entry_name = if !vfat_buf.is_empty() {
                decode_vfat_name(&vfat_buf)
            } else {
                let sfn: &[u8; MAX_SFN_LEN] = &buf[off..off + MAX_SFN_LEN]
                    .try_into()
                    .unwrap_or([b' '; MAX_SFN_LEN]);
                decode_sfn(sfn)
            };

            if entry_name.eq_ignore_ascii_case(name) {
                let mut sfn_entry: [u8; DIR_ENTRY_SIZE] = buf
                    .get(off..off + DIR_ENTRY_SIZE)
                    .ok_or(VfsError::IOError)?
                    .try_into()
                    .map_err(|_| VfsError::IOError)?;
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
        if next >= EOC_MARKER {
            break;
        }
        cluster = next;
    }
    Err(VfsError::NotFound)
}

pub(super) fn update_entry_cluster_and_size(
    sb: &Fat32SuperBlock,
    dir_clus: u32,
    name: &str,
    new_clus: Option<u32>,
    new_size: Option<u32>,
) -> Result<(), VfsError> {
    find_and_update_entry(sb, dir_clus, name, |entry| {
        if let Some(c) = new_clus {
            set_first_clus_in_entry(entry, c);
        }
        if let Some(s) = new_size {
            set_file_size_in_entry(entry, s);
        }
        let _ = set_timestamps(entry);
    })
}

pub(super) fn remove_dir_entries(
    sb: &Fat32SuperBlock,
    dir_clus: u32,
    name: &str,
) -> Result<(), VfsError> {
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
            if first == DIR_END {
                return Err(VfsError::NotFound);
            }
            if first == DIR_DELETED {
                i += 1;
                continue;
            }
            let attr = buf[off + 0x0B];
            if attr == ATTR_LONG_NAME {
                i += 1;
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                i += 1;
                continue;
            }

            let mut chain_start = i;
            while chain_start > 0
                && buf[(chain_start - 1) * DIR_ENTRY_SIZE + 0x0B] == ATTR_LONG_NAME
            {
                chain_start -= 1;
            }

            let mut vfat_buf: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::new();
            for j in chain_start..i {
                let e: &[u8; DIR_ENTRY_SIZE] = buf
                    .get(j * DIR_ENTRY_SIZE..(j + 1) * DIR_ENTRY_SIZE)
                    .ok_or(VfsError::IOError)?
                    .try_into()
                    .map_err(|_| VfsError::IOError)?;
                vfat_buf.push(*e);
            }
            let entry_name = if !vfat_buf.is_empty() {
                decode_vfat_name(&vfat_buf)
            } else {
                let sfn = &buf[off..off + MAX_SFN_LEN]
                    .try_into()
                    .unwrap_or([b' '; MAX_SFN_LEN]);
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
        if next >= EOC_MARKER {
            break;
        }
        cluster = next;
    }
    Err(VfsError::NotFound)
}
