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

impl VfsError {
    /// Stable discriminant name for logging (matches variant identifier).
    pub fn discriminant_name(&self) -> &'static str {
        match self {
            VfsError::NotFound => "NotFound",
            VfsError::AlreadyExists => "AlreadyExists",
            VfsError::NotADirectory => "NotADirectory",
            VfsError::IsADirectory => "IsADirectory",
            VfsError::IOError => "IOError",
            VfsError::NoSpace => "NoSpace",
            VfsError::CrossDeviceLink => "CrossDeviceLink",
            VfsError::InvalidInput => "InvalidInput",
            VfsError::NameTooLong => "NameTooLong",
            VfsError::NotEmpty => "NotEmpty",
            VfsError::BadFileDescriptor => "BadFileDescriptor",
            VfsError::MountBusy => "MountBusy",
            VfsError::InvalidDevice => "InvalidDevice",
            VfsError::FileTooLarge => "FileTooLarge",
            VfsError::ReadOnly => "ReadOnly",
            VfsError::NotSupported => "NotSupported",
        }
    }

    /// Numeric discriminant (stable for serial diagnostics).
    pub fn discriminant_value(&self) -> u8 {
        match self {
            VfsError::NotFound => 0,
            VfsError::AlreadyExists => 1,
            VfsError::NotADirectory => 2,
            VfsError::IsADirectory => 3,
            VfsError::IOError => 4,
            VfsError::NoSpace => 5,
            VfsError::CrossDeviceLink => 6,
            VfsError::InvalidInput => 7,
            VfsError::NameTooLong => 8,
            VfsError::NotEmpty => 9,
            VfsError::BadFileDescriptor => 10,
            VfsError::MountBusy => 11,
            VfsError::InvalidDevice => 12,
            VfsError::FileTooLarge => 13,
            VfsError::ReadOnly => 14,
            VfsError::NotSupported => 15,
        }
    }
}

/// Free function alias for use in format strings without method syntax.
pub fn vfs_error_name(e: &VfsError) -> &'static str {
    e.discriminant_name()
}
