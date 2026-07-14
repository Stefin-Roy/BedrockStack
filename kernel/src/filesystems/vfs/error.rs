use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    IOError,
    NoSpace,
    ReadOnlyFs,
    CrossDeviceLink,
    InvalidInput,
    NameTooLong,
    NotEmpty,
    BadFileDescriptor,
    NotSupported,
    WouldBlock,
    MountBusy,
    InvalidDevice,
    NotMounted,
    FileTooLarge,
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsError::NotFound => write!(f, "not found"),
            VfsError::AlreadyExists => write!(f, "already exists"),
            VfsError::NotADirectory => write!(f, "not a directory"),
            VfsError::IsADirectory => write!(f, "is a directory"),
            VfsError::IOError => write!(f, "I/O error"),
            VfsError::NoSpace => write!(f, "no space left"),
            VfsError::ReadOnlyFs => write!(f, "read-only filesystem"),
            VfsError::CrossDeviceLink => write!(f, "cross-device link"),
            VfsError::InvalidInput => write!(f, "invalid input"),
            VfsError::NameTooLong => write!(f, "name too long"),
            VfsError::NotEmpty => write!(f, "directory not empty"),
            VfsError::BadFileDescriptor => write!(f, "bad file descriptor"),
            VfsError::NotSupported => write!(f, "operation not supported"),
            VfsError::WouldBlock => write!(f, "operation would block"),
            VfsError::MountBusy => write!(f, "mount busy"),
            VfsError::InvalidDevice => write!(f, "invalid device"),
            VfsError::NotMounted => write!(f, "not mounted"),
            VfsError::FileTooLarge => write!(f, "file too large"),
        }
    }
}
