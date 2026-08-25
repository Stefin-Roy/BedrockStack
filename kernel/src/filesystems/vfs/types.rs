use core::ops::BitOr;

use alloc::string::String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Fifo,
    CharDevice,
    BlockDevice,
    Socket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags(u32);

impl OpenFlags {
    pub const READ: OpenFlags = OpenFlags(0x0000_0001);
    pub const WRITE: OpenFlags = OpenFlags(0x0000_0002);
    pub const CREATE: OpenFlags = OpenFlags(0x0000_0004);
    pub const TRUNC: OpenFlags = OpenFlags(0x0000_0008);
    pub const APPEND: OpenFlags = OpenFlags(0x0000_0010);
    pub const EXCL: OpenFlags = OpenFlags(0x0000_0020);
    /// Fail open if the final component is a symlink.
    pub const NOFOLLOW: OpenFlags = OpenFlags(0x0000_0040);
    /// Require the target to be a directory (ENOTDIR otherwise).
    pub const DIRECTORY: OpenFlags = OpenFlags(0x0000_0080);
    /// Synchronous writes: flush filesystem state before write() returns.
    /// (Data + metadata ordering guarantee; O_DSYNC semantics.)
    pub const SYNC: OpenFlags = OpenFlags(0x0000_0100);

    /// Bits this kernel understands.  Unispace must reject unknown bits
    /// before they reach the kernel so future flags fail loudly there.
    pub const KNOWN_MASK: u32 = 0x0000_01FF;

    pub fn contains(&self, flag: OpenFlags) -> bool {
        (self.0 & flag.0) != 0
    }

    pub fn new(val: u32) -> Self {
        OpenFlags(val)
    }

    pub fn bits(&self) -> u32 {
        self.0
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
    pub mode: u32,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub file_type: FileType,
}
