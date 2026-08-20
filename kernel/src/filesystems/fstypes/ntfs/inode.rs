use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::inode::InodeOps;
use crate::filesystems::vfs::types::{DirEntry, FileType, Stat};

use super::attr::{
    ATTR_ATTRIBUTE_LIST, ATTR_DATA, ATTR_FILE_NAME, FILE_NAME_INDEX_PRESENT, FLAG_COMPRESSED,
    FLAG_ENCRYPTED, MFT_REF_MASK, parse_attr_list, parse_file_name,
};
use super::index::{IndexEntry, read_dir_entries};
use super::mount::NtfsSuperBlock;
use super::record::read_mft_record;
use super::runs::{Run, RunList, decode_mapping_pairs, read_file_at};

pub(crate) enum DataSource {
    Resident(Vec<u8>),
    Runs(Arc<RunList>, u64),
    None,
}

/// An NTFS file or directory.  Identity is the MFT record number; size,
/// type and timestamps come from the first usable $FILE_NAME attribute.
pub struct NtfsInode {
    pub(crate) sb: Arc<NtfsSuperBlock>,
    pub(crate) mft_no: u64,
    pub(crate) file_type: FileType,
    pub(crate) size: u64,
    pub(crate) mtime: u64,
    pub(crate) data: Mutex<DataSource>,
    /// True when a $DATA attribute is compressed or encrypted (unsupported).
    pub(crate) unsupported: bool,
}

fn add_data_attr(
    a: &super::attr::Attr,
    record: &[u8],
    resident: &mut Option<Vec<u8>>,
    runs: &mut Vec<Run>,
    size: &mut u64,
    init: &mut u64,
    unsupported: &mut bool,
) -> Result<(), VfsError> {
    if a.flags & (FLAG_COMPRESSED | FLAG_ENCRYPTED) != 0 {
        *unsupported = true;
        return Ok(());
    }
    if a.resident {
        *resident = Some(a.value.to_vec());
        if (a.value.len() as u64) > *size {
            *size = a.value.len() as u64;
        }
    } else {
        let pairs = record.get(a.map_off..a.map_end).ok_or(VfsError::IOError)?;
        let rl = decode_mapping_pairs(pairs, a.lowest_vcn)?;
        runs.extend(rl.runs);
        if a.real_size > *size {
            *size = a.real_size;
        }
        if a.init_size > *init {
            *init = a.init_size;
        }
    }
    Ok(())
}

impl NtfsInode {
    /// Load the inode for MFT record `mft_no` (extents via $ATTRIBUTE_LIST
    /// are followed, so large files resolve to a merged run list).
    pub fn load(sb: Arc<NtfsSuperBlock>, mft_no: u64) -> Result<Arc<Self>, VfsError> {
        let record = read_mft_record(&sb, mft_no)?;
        let attrs = super::attr::iter_attrs(&record)?;

        let mut file_type = FileType::Regular;
        let mut mtime = 0u64;
        if let Some(fn_attr) = attrs.iter().find(|a| a.attr_type == ATTR_FILE_NAME) {
            if let Ok(f) = parse_file_name(fn_attr.value) {
                file_type = if f.flags & FILE_NAME_INDEX_PRESENT != 0 {
                    FileType::Directory
                } else {
                    FileType::Regular
                };
                mtime = f.mtime;
            }
        }

        let mut resident: Option<Vec<u8>> = None;
        let mut runs: Vec<Run> = Vec::new();
        let mut size = 0u64;
        let mut init = 0u64;
        let mut unsupported = false;

        let list_attr = attrs.iter().find(|a| a.attr_type == ATTR_ATTRIBUTE_LIST);
        if let Some(list) = list_attr {
            let list_bytes =
                super::attr::read_attr_value(&*sb.device, &sb.boot, &record, list, 1 << 20)?;
            let entries = parse_attr_list(&list_bytes)?;
            for e in entries {
                if e.attr_type != ATTR_DATA || e.name.is_some() {
                    continue;
                }
                // The reference embeds the sequence number in its high 16
                // bits; only the low 48 bits are the record index.
                let rec_mft = e.mft_ref & MFT_REF_MASK;
                let rec = if rec_mft == mft_no {
                    record.clone()
                } else {
                    read_mft_record(&sb, rec_mft)?
                };
                let rec_attrs = super::attr::iter_attrs(&rec)?;
                if let Some(a) = rec_attrs.iter().find(|a| {
                    a.attr_type == ATTR_DATA && a.name.is_none() && a.lowest_vcn == e.lowest_vcn
                }) {
                    add_data_attr(
                        a,
                        &rec,
                        &mut resident,
                        &mut runs,
                        &mut size,
                        &mut init,
                        &mut unsupported,
                    )?;
                }
            }
        } else {
            for a in attrs.iter() {
                if a.attr_type == ATTR_DATA && a.name.is_none() {
                    add_data_attr(
                        a,
                        &record,
                        &mut resident,
                        &mut runs,
                        &mut size,
                        &mut init,
                        &mut unsupported,
                    )?;
                }
            }
        }

        runs.sort_by_key(|r| r.vcn);
        let data = if let Some(v) = resident {
            DataSource::Resident(v)
        } else if !runs.is_empty() {
            DataSource::Runs(Arc::new(RunList { runs }), init)
        } else {
            DataSource::None
        };

        Ok(Arc::new(NtfsInode {
            sb,
            mft_no,
            file_type,
            size,
            mtime,
            data: Mutex::new(data),
            unsupported,
        }))
    }

    fn dir_entries(&self) -> Result<Vec<IndexEntry>, VfsError> {
        let record = read_mft_record(&self.sb, self.mft_no)?;
        read_dir_entries(&self.sb, &record)
    }
}

impl InodeOps for NtfsInode {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.unsupported {
            return Err(VfsError::IOError);
        }
        if self.file_type != FileType::Regular {
            return Err(VfsError::IsADirectory);
        }
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }

        let data = self.data.lock();
        match &*data {
            DataSource::Resident(v) => {
                let start = offset as usize;
                let n = core::cmp::min(buf.len(), v.len().saturating_sub(start));
                buf[..n].copy_from_slice(&v[start..start + n]);
                Ok(n)
            }
            DataSource::Runs(runs, init) => {
                let mut done = 0usize;
                let mut pos = offset;
                while done < buf.len() && pos < self.size {
                    let want = core::cmp::min(buf.len() - done, (self.size - pos) as usize);
                    if pos >= *init {
                        buf[done..done + want].fill(0);
                    } else {
                        let inited = core::cmp::min(want, (*init - pos) as usize);
                        let n = read_file_at(
                            &*self.sb.device,
                            &self.sb.boot,
                            runs,
                            pos,
                            &mut buf[done..done + inited],
                        )?;
                        buf[done + n..done + want].fill(0);
                    }
                    done += want;
                    pos += want as u64;
                }
                Ok(done)
            }
            DataSource::None => Ok(0),
        }
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        for e in self.dir_entries()? {
            if e.name.eq_ignore_ascii_case(name) {
                return Ok(NtfsInode::load(self.sb.clone(), e.mft_ref)?);
            }
        }
        Err(VfsError::NotFound)
    }

    fn create(&self, _name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn mkdir(&self, _name: &str) -> Result<Arc<dyn InodeOps>, VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn rmdir(&self, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let mut out = Vec::new();
        for e in self.dir_entries()? {
            out.push(DirEntry {
                ino: e.mft_ref & MFT_REF_MASK,
                name: e.name,
                file_type: if e.is_dir {
                    FileType::Directory
                } else {
                    FileType::Regular
                },
            });
        }
        Ok(out)
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: self.mft_no,
            size: self.size,
            file_type: self.file_type,
            mtime: self.mtime,
        })
    }

    fn rename(&self, _old_name: &str, _new_name: &str) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn truncate(&self, _len: u64) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn file_type(&self) -> FileType {
        self.file_type
    }

    fn ino(&self) -> u64 {
        self.mft_no
    }

    fn size(&self) -> u64 {
        self.size
    }
}
