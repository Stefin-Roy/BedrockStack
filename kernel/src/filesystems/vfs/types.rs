use core::ops::BitOr;

use alloc::string::String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
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
    pub file_type: FileType,
    pub mtime: u64,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub file_type: FileType,
}
