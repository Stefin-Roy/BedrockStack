use core::ops::BitOr;

use alloc::string::String;
use alloc::vec::Vec;

/// Classification of a file system object.
///
/// `Regular` and `Directory` are the on-disk kinds.  `Device`, `Op`, and
/// `Mapped` are the synthetic kinds used by the /dev and control trees.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular = 0,
    Directory = 1,
    Device = 2,
    Op = 3,
    Mapped = 4,
}

/// Backward-compat alias so external crates (obj/, proc/) that import
/// `FileType` keep compiling without code changes.
pub type FileType = FileKind;

/// Access rights bitmask for a directory entry (wire-compatible u8).
/// R = read, W = write.  A plain `RW` value means both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RightsMask(u8);

impl RightsMask {
    pub const NONE: RightsMask = RightsMask(0);
    pub const R: RightsMask = RightsMask(1 << 0);
    pub const W: RightsMask = RightsMask(1 << 1);
    pub const RW: RightsMask = RightsMask(Self::R.0 | Self::W.0);

    pub const fn contains(self, other: RightsMask) -> bool {
        (self.0 & other.0) != 0
    }

    pub const fn intersection(self, other: RightsMask) -> RightsMask {
        RightsMask(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn from_bits(bits: u8) -> RightsMask {
        RightsMask(bits)
    }
}

impl BitOr for RightsMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        RightsMask(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags(u8);

impl OpenFlags {
    pub const READ: OpenFlags = OpenFlags(0x01);
    pub const WRITE: OpenFlags = OpenFlags(0x02);
    pub const CREATE: OpenFlags = OpenFlags(0x04);
    pub const TRUNC: OpenFlags = OpenFlags(0x08);
    pub const APPEND: OpenFlags = OpenFlags(0x10);
    pub const EXCL: OpenFlags = OpenFlags(0x20);

    pub fn contains(&self, flag: OpenFlags) -> bool {
        (self.0 & flag.0) != 0
    }

    pub fn new(val: u8) -> Self {
        OpenFlags(val)
    }
}

impl BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        OpenFlags(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SeekFrom {
    Start(u64),
    End(i64),
    Current(i64),
}

#[derive(Debug, Clone)]
pub struct Stat {
    pub ino: u64,
    pub size: u64,
    pub file_kind: FileKind,
    pub mtime: u64,
}

/// A single directory entry returned by `readdir`.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub file_kind: FileKind,
    pub rights: RightsMask,
}

impl DirEntry {
    /// Serialize one entry into `out` using the stateless binary wire format:
    /// ```text
    /// [kind u8][rights u8][ino u64 LE][name_len u16 LE][name bytes]
    /// ```
    /// The total row length is `1 + 1 + 8 + 2 + name.len()`.
    pub fn marshal(&self, out: &mut Vec<u8>) {
        out.push(self.file_kind as u8);
        out.push(self.rights.bits());
        out.extend_from_slice(&self.ino.to_le_bytes());
        let name_len = self.name.len() as u16;
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(self.name.as_bytes());
    }
}

/// Serialize an entire `Vec<DirEntry>` into a flat byte buffer.  The buffer is
/// re-built on every call (stateless) so the future read syscall can slice it
/// at row boundaries without any server-side cursor state.
pub fn marshal_dir_entries(entries: &[DirEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        entry.marshal(&mut out);
    }
    out
}

/// Return the byte offset of the *n*-th entry in a marshalled listing.
/// Returns `None` when `n > entries.len()`.  Offset is the sum of all
/// preceding row sizes, so `n == entries.len()` yields the total buffer
/// length (a valid append position).
pub fn dir_entry_offset(entries: &[DirEntry], n: usize) -> Option<usize> {
    if n > entries.len() {
        return None;
    }
    let mut offset = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        if i == n {
            return Some(offset);
        }
        let row_len = 1 + 1 + 8 + 2 + entry.name.len();
        offset = offset.checked_add(row_len)?;
    }
    Some(offset)
}
