use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use hashbrown::HashSet;

use crate::filesystems::vfs::error::VfsError;

use super::attr::{
    ATTR_BITMAP, ATTR_INDEX_ALLOCATION, ATTR_INDEX_ROOT, FILE_NAME_INDEX_PRESENT, MFT_REF_MASK,
    iter_attrs, parse_file_name, read_attr_value, u16_at, u32_at, u64_at,
};
use super::mount::NtfsSuperBlock;
use super::record::usa_fixup;
use super::runs::{RunList, decode_mapping_pairs, read_file_at};

const MAX_INDEX_ENTRIES: usize = 1_000_000;
const MAX_INDEX_DEPTH: u32 = 16;
const MAX_INDEX_BLOCKS: u64 = 1_000_000;
const MAX_BITMAP_BYTES: u64 = 16 * 1024 * 1024;

/// One directory entry decoded from an index.
pub struct IndexEntry {
    pub mft_ref: u64,
    pub name: String,
    pub namespace: u8,
    pub mtime: u64,
    pub size: u64,
    pub is_dir: bool,
    pub parent_ref: u64,
}

const INDEX_ENTRY_LAST: u16 = 0x02;
const INDEX_ENTRY_SUBNODE: u16 = 0x01;

struct IndexBlock {
    data: Vec<u8>,
    start: usize,
    end: usize,
}

fn index_header_region(data: &[u8], header_off: usize) -> Result<(usize, usize), VfsError> {
    let entries_off = u32_at(data, header_off).ok_or(VfsError::IOError)? as usize;
    let used = u32_at(data, header_off + 4).ok_or(VfsError::IOError)? as usize;
    let start = header_off + entries_off;
    let end = header_off + used;
    if start < header_off || end > data.len() || start > end {
        return Err(VfsError::IOError);
    }
    Ok((start, end))
}

/// Parse the index entries in `[start, end)`, recursing into subnode blocks
/// (guarded by `visited` and a depth cap).
fn parse_entries(
    data: &[u8],
    start: usize,
    end: usize,
    sb: &NtfsSuperBlock,
    alloc_runs: Option<&RunList>,
    visited: &mut HashSet<u64>,
    out: &mut Vec<IndexEntry>,
    depth: u32,
) -> Result<(), VfsError> {
    let mut pos = start;
    while pos + 0x10 <= end {
        if out.len() >= MAX_INDEX_ENTRIES {
            return Err(VfsError::IOError);
        }
        let e_len = u16_at(data, pos + 8).ok_or(VfsError::IOError)? as usize;
        let key_len = u16_at(data, pos + 10).ok_or(VfsError::IOError)? as usize;
        let flags = u16_at(data, pos + 12).ok_or(VfsError::IOError)?;
        if e_len < 0x10 || pos + e_len > end {
            return Err(VfsError::IOError);
        }

        if flags & INDEX_ENTRY_LAST != 0 {
            break;
        }

        if key_len > 0 && key_len <= e_len - 0x10 {
            if let Ok(f) = parse_file_name(&data[pos + 0x10..pos + 0x10 + key_len]) {
                if f.name != "." && f.name != ".." {
                    let ref64 = u64_at(data, pos).ok_or(VfsError::IOError)?;
                    out.push(IndexEntry {
                        mft_ref: ref64 & MFT_REF_MASK,
                        name: f.name,
                        namespace: f.namespace,
                        mtime: f.mtime,
                        size: f.size,
                        is_dir: f.flags & FILE_NAME_INDEX_PRESENT != 0,
                        parent_ref: f.parent_ref,
                    });
                }
            }
        }

        if flags & INDEX_ENTRY_SUBNODE != 0 && depth < MAX_INDEX_DEPTH {
            if e_len >= 0x18 {
                let sub_vcn = u64_at(data, pos + e_len - 8).ok_or(VfsError::IOError)?;
                if let Some(runs) = alloc_runs {
                    if visited.insert(sub_vcn) {
                        if let Some(block) = read_index_block(sb, runs, sub_vcn)? {
                            parse_entries(
                                &block.data,
                                block.start,
                                block.end,
                                sb,
                                alloc_runs,
                                visited,
                                out,
                                depth + 1,
                            )?;
                        }
                    }
                }
            }
        }

        pos += e_len;
    }
    Ok(())
}

/// Read one INDX block (USA-fixed), validating its magic.  Returns None for
/// blocks the bitmap marks unused.
fn read_index_block(
    sb: &NtfsSuperBlock,
    runs: &RunList,
    vcn: u64,
) -> Result<Option<IndexBlock>, VfsError> {
    let mut buf = vec![0u8; sb.boot.index_size as usize];
    let offset = vcn
        .checked_mul(sb.boot.index_size)
        .ok_or(VfsError::IOError)?;
    let n = read_file_at(&*sb.device, &sb.boot, runs, offset, &mut buf)?;
    if n < sb.boot.index_size as usize {
        return Ok(None);
    }
    usa_fixup(&mut buf, sb.boot.bytes_per_sector as usize)?;
    if u32_at(&buf, 0).ok_or(VfsError::IOError)? != 0x5844_4E49 {
        // "INDX"
        return Ok(None);
    }
    let (start, end) = index_header_region(&buf, 0x18)?;
    Ok(Some(IndexBlock { data: buf, start, end }))
}

/// List the entries of the directory whose MFT record is `record`.
/// Order is on-disk order (root entries, then subnode blocks, depth-first);
/// the VFS adds `.`/`..` itself, so they are filtered here.
pub(crate) fn read_dir_entries(
    sb: &NtfsSuperBlock,
    record: &[u8],
) -> Result<Vec<IndexEntry>, VfsError> {
    let attrs = iter_attrs(record)?;

    let root = attrs
        .iter()
        .find(|a| a.attr_type == ATTR_INDEX_ROOT && a.name.as_deref() == Some("$I30"))
        .ok_or(VfsError::IOError)?;

    let alloc = attrs.iter().find(|a| {
        a.attr_type == ATTR_INDEX_ALLOCATION && a.name.as_deref() == Some("$I30")
    });
    let bitmap = attrs
        .iter()
        .find(|a| a.attr_type == ATTR_BITMAP && a.name.as_deref() == Some("$I30"));

    // Resolve the allocation attribute's runs (and bitmap bytes), if any.
    let mut alloc_runs: Option<RunList> = None;
    let mut bitmap_bytes: Vec<u8> = Vec::new();
    if let Some(a) = alloc {
        if a.resident {
            return Err(VfsError::IOError);
        }
        let pairs = record.get(a.map_off..a.map_end).ok_or(VfsError::IOError)?;
        alloc_runs = Some(decode_mapping_pairs(pairs, a.lowest_vcn)?);
        if let Some(b) = bitmap {
            bitmap_bytes = read_attr_value(&*sb.device, &sb.boot, record, b, MAX_BITMAP_BYTES)?;
        }
    }

    let mut out = Vec::new();
    let mut visited: HashSet<u64> = HashSet::new();

    let (root_start, root_end) = index_header_region(root.value, 0x10)?;
    parse_entries(
        root.value,
        root_start,
        root_end,
        sb,
        alloc_runs.as_ref(),
        &mut visited,
        &mut out,
        0,
    )?;

    // Walk the index-allocation blocks whose bitmap bit is set.  Bound the
    // walk by the stream's real size (block units) and a hard cap, never by
    // the on-disk highest_vcn alone (which is attacker-controlled and could
    // otherwise hang the boot).
    if let Some(runs) = alloc_runs.as_ref() {
        let last_vcn = alloc.map_or(0, |a| {
            a.real_size
                .saturating_div(sb.boot.index_size)
                .saturating_sub(1)
                .min(MAX_INDEX_BLOCKS)
        });
        for vcn in 0..=last_vcn {
            let used = if bitmap_bytes.is_empty() {
                true
            } else {
                let byte = (vcn / 8) as usize;
                byte < bitmap_bytes.len() && bitmap_bytes[byte] & (1 << (vcn % 8)) != 0
            };
            if !used {
                continue;
            }
            if !visited.insert(vcn) {
                continue;
            }
            if let Some(block) = read_index_block(sb, runs, vcn)? {
                parse_entries(
                    &block.data,
                    block.start,
                    block.end,
                    sb,
                    Some(runs),
                    &mut visited,
                    &mut out,
                    0,
                )?;
            }
        }
    }

    // Drop DOS 8.3 aliases that duplicate a Win32/POSIX name.
    let mut kept: Vec<IndexEntry> = Vec::with_capacity(out.len());
    for e in out {
        let is_dup_dos = e.namespace == 2
            && kept
                .iter()
                .any(|o| o.name == e.name && o.namespace != 2);
        if !is_dup_dos {
            kept.push(e);
        }
    }
    Ok(kept)
}