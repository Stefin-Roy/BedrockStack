//! Boot-time exercise of the read-only NTFS driver (behind `selftest`).
//! Mounted by `Kernel::init` as C> when a second block device is present;
//! this walks the tree, verifies known file contents, and proves the
//! write path is rejected.

use crate::drivers::serial::SerialPort;
use crate::filesystems::vfs;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::types::{FileType, OpenFlags, SeekFrom};

const MAX_DEPTH: usize = 8;

fn print_indent(depth: usize) {
    for _ in 0..depth {
        SerialPort::puts("  ");
    }
}

fn read_whole(path: &str, max: u64) -> Result<alloc::vec::Vec<u8>, VfsError> {
    let fd = vfs::open(path, OpenFlags::READ)?;
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = vfs::read(fd, &mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if (out.len() as u64) >= max {
            break;
        }
    }
    vfs::close(fd)?;
    Ok(out)
}

fn walk(path: &str, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let entries = match vfs::readdir(path) {
        Ok(e) => e,
        Err(e) => {
            SerialPort::puts("[ntfs] readdir ");
            SerialPort::puts(path);
            SerialPort::puts(" failed: ");
            SerialPort::puts(&alloc::format!("{:?}", e));
            SerialPort::puts("\n");
            return;
        }
    };
    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        print_indent(depth);
        SerialPort::puts(&e.name);
        if e.file_type == FileType::Directory {
            SerialPort::puts("/\n");
            let child = alloc::format!("{}/{}", path, e.name);
            walk(&child, depth + 1);
        } else {
            SerialPort::puts(" (");
            SerialPort::put_u64(e.ino);
            SerialPort::puts(")\n");
        }
    }
}

fn expect(path: &str, expected: &[u8]) -> bool {
    match read_whole(path, 1 << 20) {
        Ok(data) if data == expected => {
            SerialPort::puts("[ntfs] OK   content match ");
            SerialPort::puts(path);
            SerialPort::puts("\n");
            true
        }
        Ok(data) => {
            SerialPort::puts("[ntfs] FAIL content mismatch ");
            SerialPort::puts(path);
            SerialPort::puts(" got ");
            SerialPort::put_u64(data.len() as u64);
            SerialPort::puts(" bytes\n");
            false
        }
        Err(e) => {
            SerialPort::puts("[ntfs] FAIL read ");
            SerialPort::puts(path);
            SerialPort::puts(": ");
            SerialPort::puts(&alloc::format!("{:?}", e));
            SerialPort::puts("\n");
            false
        }
    }
}

pub fn run() {
    SerialPort::puts("[ntfs] selftest: walking C>\n");
    walk("C>", 0);

    let wassup = b"Hello from NTFS! (read-only demo)\n";
    let _ = expect("C>/Yo/wassup.txt", wassup);

    // The 5 MiB pattern file: verify size and a few sampled bytes.
    match vfs::stat("C>/Yo/big.bin") {
        Ok(st) => {
            SerialPort::puts("[ntfs] stat big.bin size=");
            SerialPort::put_u64(st.size);
            SerialPort::puts(" mtime=");
            SerialPort::put_u64(st.mtime);
            SerialPort::puts("\n");
            let fd = match vfs::open("C>/Yo/big.bin", OpenFlags::READ) {
                Ok(fd) => fd,
                Err(e) => {
                    SerialPort::puts("[ntfs] FAIL open big.bin: ");
                    SerialPort::puts(&alloc::format!("{:?}", e));
                    SerialPort::puts("\n");
                    return;
                }
            };
            let mut probe = [0u8; 16];
            let _ = vfs::seek(fd, SeekFrom::Start(0));
            if vfs::read(fd, &mut probe).is_ok() && probe[0] == 0x00 {
                SerialPort::puts("[ntfs] OK   big.bin head\n");
            } else {
                SerialPort::puts("[ntfs] FAIL big.bin head\n");
            }
            let _ = vfs::seek(fd, SeekFrom::Start(st.size - 16));
            if vfs::read(fd, &mut probe).is_ok() && probe[0] == ((st.size - 16) % 251) as u8 {
                SerialPort::puts("[ntfs] OK   big.bin tail\n");
            } else {
                SerialPort::puts("[ntfs] FAIL big.bin tail\n");
            }
            let _ = vfs::close(fd);
        }
        Err(e) => {
            SerialPort::puts("[ntfs] FAIL stat big.bin: ");
            SerialPort::puts(&alloc::format!("{:?}", e));
            SerialPort::puts("\n");
        }
    }

    // Empty file reads as zero bytes.
    let _ = expect("C>/Yo/empty.txt", b"");

    // Unicode name: reachable via its UTF-8 rendering.
    let _ = expect("C>/Yo/uni-\u{540d}.txt", b"unicode name file\n");

    // The write path must be refused.
    let fd = match vfs::open("C>/Yo/wassup.txt", OpenFlags::WRITE) {
        Ok(fd) => fd,
        Err(e) => {
            SerialPort::puts("[ntfs] FAIL open for write: ");
            SerialPort::puts(&alloc::format!("{:?}", e));
            SerialPort::puts("\n");
            return;
        }
    };
    match vfs::write(fd, b"nope") {
        Err(VfsError::ReadOnly) => SerialPort::puts("[ntfs] OK   write rejected (read-only)\n"),
        Ok(_) => SerialPort::puts("[ntfs] FAIL write unexpectedly succeeded\n"),
        Err(e) => {
            SerialPort::puts("[ntfs] FAIL write returned ");
            SerialPort::puts(&alloc::format!("{:?}", e));
            SerialPort::puts("\n");
        }
    }
    let _ = vfs::close(fd);

    // Mutating namespace ops are refused too.
    let mut refuses = 0u32;
    if vfs::mkdir("C>/Yo/nope").is_err() {
        refuses += 1;
    }
    if vfs::unlink("C>/Yo/wassup.txt").is_err() {
        refuses += 1;
    }
    if vfs::truncate("C>/Yo/wassup.txt", 0).is_err() {
        refuses += 1;
    }
    SerialPort::puts("[ntfs] selftest done (");
    SerialPort::put_u64(refuses as u64);
    SerialPort::puts(" mutation ops refused)\n");
}
