//! /pci — PCI device forest.
//!
//! Synthetic VFS tree over the ambient PCI census (`pci::devices`, the raw
//! post-enumeration slice).  Each discovered function appears as
//! `/pci/<index>` (index is unique even when segment/bus/device.function
//! repeats), exposing a read-only `:info` op plus structure-only `:cfg`,
//! `:bar` and `:irq` ops.  Real config-space read/write dispatch is wired in
//! Phase 5; `:info` is already functional.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};
use crate::pci::PciDevice;

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 30;
const INO_BASE: u64 = 3000;
const INO_INFO: u64 = 100000;
const INO_CFG: u64 = 100001;
const INO_BAR: u64 = 100002;
const INO_IRQ: u64 = 100003;

static PCI_OPS: [OpDesc; 4] = [
    OpDesc {
        name: ":info",
        rights: RightsMask::R,
        doc: "Read device identity (segment:bus:device.function, IDs, class, IRQ line/pin).",
    },
    OpDesc {
        name: ":cfg",
        rights: RightsMask::RW,
        doc: "Read/write a config-space register (write request body: offset, value).",
    },
    OpDesc {
        name: ":bar",
        rights: RightsMask::R,
        doc: "Read the device's BAR layout.",
    },
    OpDesc {
        name: ":irq",
        rights: RightsMask::RW,
        doc: "Read/write the device's interrupt configuration.",
    },
];

// ── Op nodes ─────────────────────────────────────────────────────────────

/// Read-only identity op — serves a text line describing the function.
pub struct PciInfo {
    dev: PciDevice,
}

impl FileOps for PciInfo {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        INO_INFO
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let text = format!(
            "bus={:02x}:{:02x}.{} seg={} ven={:04x} dev={:04x} rev={:02x} class={:02x}{:02x}{:02x} irq={}/{}\n",
            self.dev.bus,
            self.dev.device,
            self.dev.function,
            self.dev.segment,
            self.dev.vendor_id,
            self.dev.device_id,
            self.dev.revision,
            self.dev.class,
            self.dev.subclass,
            self.dev.prog_if,
            self.dev.interrupt_line,
            self.dev.interrupt_pin,
        );
        Ok(super::serve_text(text.as_bytes(), offset, buf))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_INFO, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

/// Structure-only config/bar/irq op (default read/write -> NotSupported).
pub struct PciOp {
    ino: u64,
}

impl FileOps for PciOp {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

// ── Per-function directory ───────────────────────────────────────────────

/// `/pci/<index>` — one discovered PCI function.
pub struct PciEntry {
    idx: u64,
    dev: PciDevice,
}

impl FileOps for PciEntry {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_BASE + self.idx
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry { ino: INO_INFO, name: String::from(":info"), file_kind: FileKind::Op, rights: RightsMask::R },
            DirEntry { ino: INO_CFG, name: String::from(":cfg"), file_kind: FileKind::Op, rights: RightsMask::RW },
            DirEntry { ino: INO_BAR, name: String::from(":bar"), file_kind: FileKind::Op, rights: RightsMask::R },
            DirEntry { ino: INO_IRQ, name: String::from(":irq"), file_kind: FileKind::Op, rights: RightsMask::RW },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            ":info" => Ok(Arc::new(PciInfo { dev: self.dev })),
            ":cfg" => Ok(Arc::new(PciOp { ino: INO_CFG })),
            ":bar" => Ok(Arc::new(PciOp { ino: INO_BAR })),
            ":irq" => Ok(Arc::new(PciOp { ino: INO_IRQ })),
            _ => Err(VfsError::NotFound),
        }
    }

    fn ops(&self) -> &'static [OpDesc] {
        &PCI_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino(), size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /pci — PCI device forest.
pub struct PciRoot;

impl FileOps for PciRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let count = crate::pci::devices().len();
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            entries.push(DirEntry {
                ino: INO_BASE + i as u64,
                name: i.to_string(),
                file_kind: FileKind::Directory,
                rights: RightsMask::R,
            });
        }
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let idx: usize = name.parse().map_err(|_| VfsError::NotFound)?;
        let dev = crate::pci::devices().get(idx).copied().ok_or(VfsError::NotFound)?;
        Ok(Arc::new(PciEntry { idx: idx as u64, dev }))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_ROOT, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the canonical `/pci` root.
pub fn pci_root() -> Arc<dyn FileOps> {
    Arc::new(PciRoot)
}
