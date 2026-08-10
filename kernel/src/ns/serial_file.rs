//! `/console` — the kernel serial console as a write-only file.
//!
//! Wraps `services::serial` (the raw COM path) behind `FileOps`, replacing
//! the old cap-mediated serial node. This is the ambient console: writes go
//! straight to the COM port. It is kernel-endowed at the root namespace, so
//! any task may write to the console without a capability.

use alloc::sync::Arc;

use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::file_ops::FileOps;
use crate::filesystems::vfs::types::{FileKind, Stat};
use crate::services::serial::{init as serial_console, SerialConsole};

/// Inode for the console file (outside every synthetic tree's range).
const INO_CONSOLE: u64 = 900000;

pub struct ConsoleFile {
    console: &'static dyn SerialConsole,
}

impl FileOps for ConsoleFile {
    fn file_kind(&self) -> FileKind {
        FileKind::Device
    }

    fn ino(&self) -> u64 {
        INO_CONSOLE
    }

    fn write(&self, _offset: u64, data: &[u8]) -> Result<usize, VfsError> {
        for &b in data {
            self.console.putc(b);
        }
        Ok(data.len())
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: INO_CONSOLE,
            size: 0,
            file_kind: FileKind::Device,
            mtime: 0,
        })
    }
}

/// Construct the canonical `/console` file.
pub fn console_file() -> Arc<dyn FileOps> {
    Arc::new(ConsoleFile {
        console: serial_console(),
    })
}
