//! /dev — device family tree.
//!
//! Synthetic VFS tree exposing the kernel's device families as directories
//! of op files.  `/dev/block/<n>` enumerates the ambient `BLOCK_DEVICES`
//! registry (one entry per storage device, AHCI + xHCI-attached alike); each
//! entry carries a functional `:geometry` op plus structure-only `:ctl`,
//! `:map`, `:read` and `:write` ops (block I/O dispatch, with park-on-read,
//! is wired in Phase 5).  The serial/ps2/input/audio/pci families are static
//! control directories whose `:ctl` dispatch lands in Phase 5.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 60;
const INO_BLOCK: u64 = 70;
const INO_SERIAL: u64 = 71;
const INO_PS2: u64 = 72;
const INO_INPUT: u64 = 73;
const INO_AUDIO: u64 = 74;
const INO_DEVPCI: u64 = 75;

// Block device entries and their op files.
const INO_BLOCK_ENTRY: u64 = 7000;
const INO_BLOCK_OP: u64 = 71000; // + idx*8: geom=+0 ctl=+1 map=+2 read=+3 write=+4
const INO_GEOM: u64 = 0;
const INO_CTL: u64 = 1;
const INO_MAP: u64 = 2;
const INO_READ: u64 = 3;
const INO_WRITE: u64 = 4;

const INO_SERIAL_STATUS: u64 = 72000;
const INO_SERIAL_CTL: u64 = 72001;
const INO_PS2_STATUS: u64 = 73000;
const INO_PS2_CTL: u64 = 73001;
const INO_INPUT_CTL: u64 = 74000;
const INO_AUDIO_CTL: u64 = 75000;
const INO_AUDIO_MAP: u64 = 75001;
const INO_DEVPCI_CTL: u64 = 76000;

// ── Op descriptor tables ─────────────────────────────────────────────────

static BLOCK_OPS: [OpDesc; 5] = [
    OpDesc { name: ":geometry", rights: RightsMask::R, doc: "Read the device's model string and sector count." },
    OpDesc { name: ":ctl", rights: RightsMask::W, doc: "Device control (write request body)." },
    OpDesc { name: ":map", rights: RightsMask::W, doc: "Map a range of the device (write request body)." },
    OpDesc { name: ":read", rights: RightsMask::RW, doc: "Read sectors from the device (write request body: lba, count)." },
    OpDesc { name: ":write", rights: RightsMask::RW, doc: "Write sectors to the device (write request body: lba, data)." },
];

static SERIAL_OPS: [OpDesc; 2] = [
    OpDesc { name: ":status", rights: RightsMask::R, doc: "Read the serial port state." },
    OpDesc { name: ":ctl", rights: RightsMask::W, doc: "Serial port control (write request body)." },
];

static PS2_OPS: [OpDesc; 2] = [
    OpDesc { name: ":status", rights: RightsMask::R, doc: "Read the PS/2 controller state." },
    OpDesc { name: ":ctl", rights: RightsMask::W, doc: "PS/2 controller control (write request body)." },
];

static INPUT_OPS: [OpDesc; 1] = [
    OpDesc { name: ":ctl", rights: RightsMask::W, doc: "Input-layer control (write request body)." },
];

static AUDIO_OPS: [OpDesc; 2] = [
    OpDesc { name: ":ctl", rights: RightsMask::W, doc: "Audio controller control (write request body)." },
    OpDesc { name: ":map", rights: RightsMask::W, doc: "Map the audio buffer (write request body)." },
];

static DEVPCI_OPS: [OpDesc; 1] = [
    OpDesc { name: ":ctl", rights: RightsMask::W, doc: "PCI bus control (write request body)." },
];

// ── Generic structure-only op node ───────────────────────────────────────

/// A structure-only op file (`:ctl`, `:map`, block I/O).  Real dispatch is
/// wired in Phase 5; read/write fall through to `NotSupported`.
pub struct DevOp {
    ino: u64,
}

impl FileOps for DevOp {
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

// ── Functional op nodes ──────────────────────────────────────────────────

/// `:geometry` on a block device — functional read.
pub struct BlockGeom {
    ino: u64,
    dev: Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>,
}

impl FileOps for BlockGeom {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let text = format!("model={}\nsectors={}\n", self.dev.model_string(), self.dev.sector_count());
        Ok(super::serve_text(text.as_bytes(), offset, buf))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

/// `:status` op for the static families — functional read.
pub struct FamilyStatus {
    ino: u64,
    text: &'static str,
}

impl FileOps for FamilyStatus {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        Ok(super::serve_text(self.text.as_bytes(), offset, buf))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

// ── Block device entry ───────────────────────────────────────────────────

/// `/dev/block/<n>` — one registered block device.
pub struct BlockEntry {
    idx: u64,
    dev: Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>,
}

impl BlockEntry {
    fn op_ino(&self, which: u64) -> u64 {
        INO_BLOCK_OP + self.idx * 8 + which
    }
}

impl FileOps for BlockEntry {
    fn file_kind(&self) -> FileKind {
        FileKind::Device
    }

    fn ino(&self) -> u64 {
        INO_BLOCK_ENTRY + self.idx
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry { ino: self.op_ino(INO_GEOM), name: String::from(":geometry"), file_kind: FileKind::Op, rights: RightsMask::R },
            DirEntry { ino: self.op_ino(INO_CTL), name: String::from(":ctl"), file_kind: FileKind::Op, rights: RightsMask::W },
            DirEntry { ino: self.op_ino(INO_MAP), name: String::from(":map"), file_kind: FileKind::Op, rights: RightsMask::W },
            DirEntry { ino: self.op_ino(INO_READ), name: String::from(":read"), file_kind: FileKind::Op, rights: RightsMask::RW },
            DirEntry { ino: self.op_ino(INO_WRITE), name: String::from(":write"), file_kind: FileKind::Op, rights: RightsMask::RW },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            ":geometry" => Ok(Arc::new(BlockGeom { ino: self.op_ino(INO_GEOM), dev: self.dev.clone() })),
            ":ctl" => Ok(Arc::new(DevOp { ino: self.op_ino(INO_CTL) })),
            ":map" => Ok(Arc::new(DevOp { ino: self.op_ino(INO_MAP) })),
            ":read" => Ok(Arc::new(DevOp { ino: self.op_ino(INO_READ) })),
            ":write" => Ok(Arc::new(DevOp { ino: self.op_ino(INO_WRITE) })),
            _ => Err(VfsError::NotFound),
        }
    }

    fn ops(&self) -> &'static [OpDesc] {
        &BLOCK_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino(), size: self.dev.sector_count().saturating_mul(512), file_kind: FileKind::Device, mtime: 0 })
    }
}

// ── Static family directories ────────────────────────────────────────────

/// A static `/dev/<family>` directory with a fixed op table.
pub struct FamilyDir {
    ino: u64,
    ops: &'static [OpDesc],
    // (op name, op ino, op kind rights) in readdir order.
    children: &'static [(&'static str, u64, RightsMask)],
    // Op names served by a functional `:status`-style node.
    status: Option<(&'static str, u64, &'static str)>,
}

impl FileOps for FamilyDir {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(self
            .children
            .iter()
            .map(|(name, ino, rights)| DirEntry {
                ino: *ino,
                name: String::from(*name),
                file_kind: FileKind::Op,
                rights: *rights,
            })
            .collect())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        if let Some((op, ino, text)) = self.status {
            if name == op {
                return Ok(Arc::new(FamilyStatus { ino, text }));
            }
        }
        self.children
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, ino, _)| Arc::new(DevOp { ino: *ino }) as Arc<dyn FileOps>)
            .ok_or(VfsError::NotFound)
    }

    fn ops(&self) -> &'static [OpDesc] {
        self.ops
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

// ── Block directory ──────────────────────────────────────────────────────

/// `/dev/block` — enumerates the ambient block-device registry.
pub struct BlockDir;

impl FileOps for BlockDir {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_BLOCK
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let devs = crate::filesystems::blockdriver::driver::BLOCK_DEVICES.lock();
        let mut entries = Vec::with_capacity(devs.len());
        for i in 0..devs.len() {
            entries.push(DirEntry {
                ino: INO_BLOCK_ENTRY + i as u64,
                name: i.to_string(),
                file_kind: FileKind::Device,
                rights: RightsMask::RW,
            });
        }
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let idx: usize = name.parse().map_err(|_| VfsError::NotFound)?;
        let devs = crate::filesystems::blockdriver::driver::BLOCK_DEVICES.lock();
        let dev = devs.get(idx).cloned().ok_or(VfsError::NotFound)?;
        Ok(Arc::new(BlockEntry { idx: idx as u64, dev }))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_BLOCK, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /dev — device family tree.
pub struct DevRoot;

impl FileOps for DevRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let mut entries = vec![
            DirEntry { ino: INO_BLOCK, name: String::from("block"), file_kind: FileKind::Directory, rights: RightsMask::RW },
            DirEntry { ino: INO_SERIAL, name: String::from("serial"), file_kind: FileKind::Directory, rights: RightsMask::R },
            DirEntry { ino: INO_PS2, name: String::from("ps2"), file_kind: FileKind::Directory, rights: RightsMask::R },
            DirEntry { ino: INO_INPUT, name: String::from("input"), file_kind: FileKind::Directory, rights: RightsMask::R },
        ];
        #[cfg(target_arch = "x86_64")]
        entries.push(DirEntry { ino: INO_AUDIO, name: String::from("audio"), file_kind: FileKind::Directory, rights: RightsMask::R });
        entries.push(DirEntry { ino: INO_DEVPCI, name: String::from("pci"), file_kind: FileKind::Directory, rights: RightsMask::R });
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            "block" => Ok(Arc::new(BlockDir)),
            "serial" => Ok(Arc::new(FamilyDir {
                ino: INO_SERIAL,
                ops: &SERIAL_OPS,
                children: &[
                    (":status", INO_SERIAL_STATUS, RightsMask::R),
                    (":ctl", INO_SERIAL_CTL, RightsMask::W),
                ],
                status: Some((":status", INO_SERIAL_STATUS, "port=COM1\n")),
            })),
            "ps2" => Ok(Arc::new(FamilyDir {
                ino: INO_PS2,
                ops: &PS2_OPS,
                children: &[
                    (":status", INO_PS2_STATUS, RightsMask::R),
                    (":ctl", INO_PS2_CTL, RightsMask::W),
                ],
                status: Some((":status", INO_PS2_STATUS, "controller=8042\n")),
            })),
            "input" => Ok(Arc::new(FamilyDir {
                ino: INO_INPUT,
                ops: &INPUT_OPS,
                children: &[(":ctl", INO_INPUT_CTL, RightsMask::W)],
                status: None,
            })),
            #[cfg(target_arch = "x86_64")]
            "audio" => Ok(Arc::new(FamilyDir {
                ino: INO_AUDIO,
                ops: &AUDIO_OPS,
                children: &[
                    (":ctl", INO_AUDIO_CTL, RightsMask::W),
                    (":map", INO_AUDIO_MAP, RightsMask::W),
                ],
                status: None,
            })),
            "pci" => Ok(Arc::new(FamilyDir {
                ino: INO_DEVPCI,
                ops: &DEVPCI_OPS,
                children: &[(":ctl", INO_DEVPCI_CTL, RightsMask::W)],
                status: None,
            })),
            _ => Err(VfsError::NotFound),
        }
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_ROOT, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the canonical `/dev` root.
pub fn dev_root() -> Arc<dyn FileOps> {
    Arc::new(DevRoot)
}
