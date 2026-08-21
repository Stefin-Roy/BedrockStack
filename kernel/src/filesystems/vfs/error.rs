use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    IOError,
    NoSpace,
    CrossDeviceLink,
    InvalidInput,
    NameTooLong,
    NotEmpty,
    BadFileDescriptor,
    MountBusy,
    InvalidDevice,
    FileTooLarge,
    ReadOnly,
    NotSupported,
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
            VfsError::CrossDeviceLink => write!(f, "cross-device link"),
            VfsError::InvalidInput => write!(f, "invalid input"),
            VfsError::NameTooLong => write!(f, "name too long"),
            VfsError::NotEmpty => write!(f, "directory not empty"),
            VfsError::BadFileDescriptor => write!(f, "bad file descriptor"),
            VfsError::MountBusy => write!(f, "mount busy"),
            VfsError::InvalidDevice => write!(f, "invalid device"),
            VfsError::FileTooLarge => write!(f, "file too large"),
            VfsError::ReadOnly => write!(f, "read-only file system"),
            VfsError::NotSupported => write!(f, "operation not supported"),
        }
    }
}
