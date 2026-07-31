use alloc::format;
use alloc::sync::Arc;
use core::fmt::Write;

use crate::drivers::serial::SerialPort;
use crate::filesystems::blockdriver::driver::BLOCK_DEVICES;
use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::filesystems::partition::{self, PartitionDevice};
use crate::filesystems::vfs;
use crate::filesystems::vfs::types::*;
use framebuffer::Framebuffer;
use super::Module;

struct WalkStats {
    dirs: u64,
    files: u64,
    txt_files: u64,
}

impl WalkStats {
    const fn new() -> Self {
        WalkStats { dirs: 0, files: 0, txt_files: 0 }
    }
}

fn indent(depth: usize) {
    for _ in 0..depth {
        SerialPort::puts("  ");
    }
}

fn read_txt(path: &str, depth: usize) {
    const MAX: usize = 4096;
    let fd = match vfs::open(path, OpenFlags::READ) {
        Ok(fd) => fd,
        Err(_) => {
            indent(depth);
            SerialPort::puts("  [CANNOT OPEN]\n");
            return;
        }
    };
    let mut buf = [0u8; MAX];
    let n = match vfs::read(fd, &mut buf) {
        Ok(n) => n,
        Err(_) => {
            vfs::close(fd).ok();
            indent(depth);
            SerialPort::puts("  [READ ERROR]\n");
            return;
        }
    };
    vfs::close(fd).ok();
    if n == 0 {
        indent(depth);
        SerialPort::puts("  [EMPTY]\n");
        return;
    }
    indent(depth);
    SerialPort::puts("  CONTENTS:\n");
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("<binary data>");
    for line in text.lines() {
        indent(depth + 1);
        SerialPort::puts(line);
        SerialPort::puts("\n");
    }
    if n >= MAX {
        indent(depth + 1);
        SerialPort::puts("... (truncated at 4096 bytes)\n");
    }
}

fn walk_dir(path: &str, depth: usize, stats: &mut WalkStats) {
    let entries = match vfs::readdir(path) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in &entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let full = if path.ends_with('>') || path.ends_with('/') {
            format!("{}{}", path, entry.name)
        } else {
            format!("{}/{}", path, entry.name)
        };
        match entry.file_type {
            FileType::Directory => {
                stats.dirs += 1;
                indent(depth);
                SerialPort::puts("[DIR]  ");
                SerialPort::puts(&entry.name);
                SerialPort::puts("\n");
                walk_dir(&full, depth + 1, stats);
            }
            FileType::Regular => {
                stats.files += 1;
                indent(depth);
                SerialPort::puts("[FILE] ");
                SerialPort::puts(&entry.name);
                if let Ok(st) = vfs::stat(&full) {
                    SerialPort::puts(" (");
                    SerialPort::put_u64(st.size);
                    SerialPort::puts(" bytes)");
                }
                SerialPort::puts("\n");
                if entry.name.ends_with(".txt") || entry.name.ends_with(".TXT") {
                    stats.txt_files += 1;
                    read_txt(&full, depth + 1);
                }
            }
        }
    }
}

fn find_free_drive(start: char) -> Option<char> {
    for c in (start as u8..=b'Z').map(|b| b as char) {
        if vfs::stat(&format!("{}>", c)).is_err() {
            return Some(c);
        }
    }
    None
}

fn probe_device(device: Arc<dyn BlockDevice>, stats: &mut WalkStats) {
    let model = device.model_string();
    let sectors = device.sector_count();
    SerialPort::puts("[FAT32_LS] Device: ");
    SerialPort::puts(model);
    SerialPort::puts(" (");
    SerialPort::put_u64(sectors);
    SerialPort::puts(" sectors)\n");

    let table = match partition::probe(device.clone()) {
        Ok(t) => t,
        Err(e) => {
            SerialPort::puts("[FAT32_LS]   probe: ");
            SerialPort::puts(e);
            SerialPort::puts("\n");
            return;
        }
    };
    let parts = table.partitions();
    SerialPort::puts("[FAT32_LS]   partitions: ");
    SerialPort::put_u64(parts.len() as u64);
    SerialPort::puts("\n");

    for part in parts {
        if part.is_extended {
            SerialPort::puts("[FAT32_LS]     #");
            SerialPort::put_u64(part.number as u64);
            SerialPort::puts(": extended, skip\n");
            continue;
        }
        let letter = match find_free_drive('C') {
            Some(l) => l,
            None => {
                SerialPort::puts("[FAT32_LS]     no free drive letters\n");
                break;
            }
        };
        let part_dev: Arc<dyn BlockDevice> = Arc::new(PartitionDevice::new(device.clone(), part));
        SerialPort::puts("[FAT32_LS]     #");
        SerialPort::put_u64(part.number as u64);
        SerialPort::puts(" -> ");
        SerialPort::putc(letter as u8);
        SerialPort::puts("> ... ");
        match vfs::mount("fat32", Some(part_dev), letter) {
            Ok(()) => {
                SerialPort::puts("mounted\n");
                let root = format!("{}>", letter);
                walk_dir(&root, 2, stats);
                if let Err(e) = vfs::unmount(letter) {
                    let mut port = SerialPort::new();
                    write!(port, "[FAT32_LS]     unmount {}>: {}\n", letter, e).ok();
                } else {
                    SerialPort::puts("[FAT32_LS]     unmounted ");
                    SerialPort::putc(letter as u8);
                    SerialPort::puts(">\n");
                }
            }
            Err(e) => {
                let mut port = SerialPort::new();
                write!(port, "failed: {}\n", e).ok();
            }
        }
    }
}

pub struct Fat32Ls;

impl Module for Fat32Ls {
    fn name(&self) -> &str {
        "fat32_ls"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn init(&self, _display: &mut Framebuffer) -> Result<(), &'static str> {
        SerialPort::puts("[FAT32_LS] === Recursive FAT32 Directory Viewer ===\n");

        let mut stats = WalkStats::new();
        let mut scanned = 0u64;

        // Walk already-mounted drives that have block devices (e.g. B>)
        for (letter, mount) in crate::filesystems::vfs::DRIVE_MAP.iter() {
            if mount.device.is_some() {
                scanned += 1;
                SerialPort::puts("[FAT32_LS] Scanning mounted ");
                SerialPort::putc(letter as u8);
                SerialPort::puts(">\n");
                walk_dir(&format!("{}>", letter), 1, &mut stats);
            }
        }

        // Probe all block devices for FAT32 partitions
        let devices = BLOCK_DEVICES.lock();
        if !devices.is_empty() {
            SerialPort::puts("[FAT32_LS] Probing ");
            SerialPort::put_u64(devices.len() as u64);
            SerialPort::puts(" block device(s)\n");
            for dev in devices.iter() {
                probe_device(dev.clone(), &mut stats);
            }
        } else {
            SerialPort::puts("[FAT32_LS] No block devices in registry\n");
        }

        SerialPort::puts("[FAT32_LS] === Summary ===\n");
        SerialPort::puts("[FAT32_LS]   Volumes: ");
        SerialPort::put_u64(scanned);
        SerialPort::puts("\n");
        SerialPort::puts("[FAT32_LS]   Dirs: ");
        SerialPort::put_u64(stats.dirs);
        SerialPort::puts(", Files: ");
        SerialPort::put_u64(stats.files);
        SerialPort::puts(", .txt read: ");
        SerialPort::put_u64(stats.txt_files);
        SerialPort::puts("\n");
        SerialPort::puts("[FAT32_LS] Done.\n");

        Ok(())
    }
}
