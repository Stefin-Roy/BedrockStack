use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use framebuffer::Framebuffer;
use crate::drivers::serial::SerialPort;
use crate::filesystems::vfs;
use crate::filesystems::vfs::error::VfsError;
use crate::filesystems::vfs::types::*;
use super::Module;

static PASS: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);

macro_rules! t {
    ($name:expr, $body:expr) => {
        {
            let mut port = SerialPort::new();
            write!(port, "[FAT32TEST] {:35} ", $name).ok();
            match $body {
                Ok(()) => {
                    write!(port, "PASS\n").ok();
                    PASS.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    write!(port, "FAIL: {}\n", e).ok();
                    FAIL.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn try_open(path: &str, flags: OpenFlags) -> Result<u32, &'static str> {
    vfs::open(path, flags).map_err(|_| "open failed")
}

fn try_close(fd: u32) -> Result<(), &'static str> {
    vfs::close(fd).map_err(|_| "close failed")
}

fn try_write(fd: u32, buf: &[u8]) -> Result<(), &'static str> {
    let n = vfs::write(fd, buf).map_err(|_| "write failed")?;
    if n != buf.len() {
        return Err("short write");
    }
    Ok(())
}

fn try_read(fd: u32, buf: &mut [u8]) -> Result<usize, &'static str> {
    vfs::read(fd, buf).map_err(|_| "read failed")
}

fn try_mkdir(path: &str) -> Result<(), &'static str> {
    vfs::mkdir(path).map_err(|_| "mkdir failed")
}

fn try_rmdir(path: &str) -> Result<(), &'static str> {
    vfs::rmdir(path).map_err(|_| "rmdir failed")
}

fn try_unlink(path: &str) -> Result<(), &'static str> {
    vfs::unlink(path).map_err(|_| "unlink failed")
}

fn try_stat(path: &str) -> Result<Stat, &'static str> {
    vfs::stat(path).map_err(|_| "stat failed")
}

fn try_rename(old: &str, new: &str) -> Result<(), &'static str> {
    vfs::rename(old, new).map_err(|_| "rename failed")
}

fn try_chdir(path: &str) -> Result<(), &'static str> {
    vfs::chdir(path).map_err(|_| "chdir failed")
}

fn try_readdir(path: &str) -> Result<Vec<DirEntry>, &'static str> {
    vfs::readdir(path).map_err(|_| "readdir failed")
}

fn try_truncate(path: &str, len: u64) -> Result<(), &'static str> {
    vfs::truncate(path, len).map_err(|_| "truncate failed")
}

fn try_seek(fd: u32, whence: SeekFrom) -> Result<u64, &'static str> {
    vfs::seek(fd, whence).map_err(|_| "seek failed")
}

fn try_getcwd() -> Result<String, &'static str> {
    vfs::getcwd().map_err(|_| "getcwd failed")
}

fn rd(path: &str) -> Result<(), &'static str> {
    let entries = try_readdir(path)?;
    for e in &entries {
        if e.name == "." || e.name == ".." { continue; }
        let child = if path.ends_with('/') {
            alloc::format!("{}{}", path, e.name)
        } else {
            alloc::format!("{}/{}", path, e.name)
        };
        if e.file_type == FileType::Directory {
            rd(&child)?;
            try_rmdir(&child)?;
        } else {
            try_unlink(&child)?;
        }
    }
    Ok(())
}

fn cleanup_workdir() {
    let _ = rd("B>fat32_test");
    let _ = try_rmdir("B>fat32_test");
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

fn test_create_unlink() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/cu")?;
    let fd = try_open("B>fat32_test/cu/file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd)?;
    let st = try_stat("B>fat32_test/cu/file.txt")?;
    if st.file_type != FileType::Regular {
        return Err("stat returned non-regular after create");
    }
    if st.size != 0 {
        return Err("new file size should be 0");
    }
    try_unlink("B>fat32_test/cu/file.txt")?;
    match vfs::stat("B>fat32_test/cu/file.txt") {
        Err(VfsError::NotFound) => {}
        _ => return Err("stat should return NotFound after unlink"),
    }
    rd("B>fat32_test/cu")?;
    try_rmdir("B>fat32_test/cu")
}

fn test_write_read() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/wr")?;
    let fd = try_open("B>fat32_test/wr/data.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    try_write(fd, b"HelloFAT32")?;
    try_seek(fd, SeekFrom::Start(0))?;
    let mut buf = [0u8; 10];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 10 || &buf != b"HelloFAT32" {
        return Err("write/read content mismatch");
    }
    rd("B>fat32_test/wr")?;
    try_rmdir("B>fat32_test/wr")
}

fn test_seek() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/seek")?;
    let fd = try_open("B>fat32_test/seek/data.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    try_write(fd, b"0123456789")?;
    try_seek(fd, SeekFrom::Start(3))?;
    let mut buf = [0u8; 4];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 4 || &buf != b"3456" {
        return Err("seek + read gave wrong data");
    }
    rd("B>fat32_test/seek")?;
    try_rmdir("B>fat32_test/seek")
}

fn test_append() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/append")?;
    let fd = try_open("B>fat32_test/append/data.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::READ)?;
    try_write(fd, b"Hello")?;
    try_write(fd, b"World")?;
    try_seek(fd, SeekFrom::Start(0))?;
    let mut buf = [0u8; 10];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 10 || &buf != b"HelloWorld" {
        return Err("APPEND content mismatch");
    }
    rd("B>fat32_test/append")?;
    try_rmdir("B>fat32_test/append")
}

fn test_truncate() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/trunc")?;
    let fd = try_open("B>fat32_test/trunc/data.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"HelloFAT32")?;
    try_close(fd)?;
    try_truncate("B>fat32_test/trunc/data.txt", 0)?;
    let st = try_stat("B>fat32_test/trunc/data.txt")?;
    if st.size != 0 {
        return Err("size not 0 after truncate");
    }
    let fd = try_open("B>fat32_test/trunc/data.txt", OpenFlags::READ)?;
    let mut buf = [0u8; 4];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 0 {
        return Err("should read 0 bytes after truncate");
    }
    rd("B>fat32_test/trunc")?;
    try_rmdir("B>fat32_test/trunc")
}

fn test_mkdir_rmdir() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/mr")?;
    try_mkdir("B>fat32_test/mr/sub")?;
    let st = try_stat("B>fat32_test/mr/sub")?;
    if st.file_type != FileType::Directory {
        return Err("stat on subdir returned wrong type");
    }
    try_rmdir("B>fat32_test/mr/sub")?;
    match vfs::stat("B>fat32_test/mr/sub") {
        Err(VfsError::NotFound) => {}
        _ => return Err("subdir should not exist after rmdir"),
    }
    rd("B>fat32_test/mr")?;
    try_rmdir("B>fat32_test/mr")
}

fn test_mkdir_nonempty() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/ne")?;
    let fd = try_open("B>fat32_test/ne/f", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd)?;
    match vfs::rmdir("B>fat32_test/ne") {
        Err(VfsError::NotEmpty) => {}
        Ok(()) => return Err("rmdir on non-empty should fail"),
        _ => return Err("wrong error for rmdir non-empty"),
    }
    rd("B>fat32_test/ne")?;
    try_rmdir("B>fat32_test/ne")
}

fn test_nested_dirs() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/nd")?;
    try_mkdir("B>fat32_test/nd/a")?;
    try_mkdir("B>fat32_test/nd/a/b")?;
    try_mkdir("B>fat32_test/nd/a/b/c")?;
    let st = try_stat("B>fat32_test/nd/a/b/c")?;
    if st.file_type != FileType::Directory {
        return Err("deeply nested dir stat returned wrong type");
    }
    // Create a file in the deepest directory
    let fd = try_open("B>fat32_test/nd/a/b/c/file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"deep")?;
    try_close(fd)?;
    let st = try_stat("B>fat32_test/nd/a/b/c/file.txt")?;
    if st.size != 4 {
        return Err("nested file has wrong size");
    }
    rd("B>fat32_test/nd")?;
    try_rmdir("B>fat32_test/nd")
}

fn test_readdir() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/rd")?;
    let f1 = try_open("B>fat32_test/rd/alpha.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(f1)?;
    let f2 = try_open("B>fat32_test/rd/beta.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(f2)?;
    let f3 = try_open("B>fat32_test/rd/gamma.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(f3)?;
    let entries = try_readdir("B>fat32_test/rd")?;
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    if !names.contains(&".") {
        return Err("readdir missing .");
    }
    if !names.contains(&"..") {
        return Err("readdir missing ..");
    }
    if !names.contains(&"alpha.txt") {
        return Err("readdir missing alpha.txt");
    }
    if !names.contains(&"beta.txt") {
        return Err("readdir missing beta.txt");
    }
    if !names.contains(&"gamma.txt") {
        return Err("readdir missing gamma.txt");
    }
    rd("B>fat32_test/rd")?;
    try_rmdir("B>fat32_test/rd")
}

fn test_dotdot() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/dd")?;
    try_mkdir("B>fat32_test/dd/sub")?;
    try_chdir("B>fat32_test/dd/sub")?;
    let cwd = try_getcwd()?;
    if cwd != "B>fat32_test/dd/sub" {
        try_chdir("B>")?;
        return Err("getcwd after chdir");
    }
    try_chdir("..")?;
    let cwd2 = try_getcwd()?;
    if cwd2 != "B>fat32_test/dd" {
        try_chdir("B>")?;
        return Err("getcwd after ..");
    }
    try_chdir("..")?;
    let cwd3 = try_getcwd()?;
    if cwd3 != "B>fat32_test" {
        try_chdir("B>")?;
        return Err("getcwd after ../..");
    }
    // Clamped at root
    try_chdir("..")?;
    try_chdir("..")?;
    let cwd4 = try_getcwd()?;
    if cwd4 != "B>" {
        try_chdir("B>")?;
        return Err("getcwd should clamp at B>");
    }
    // Restore CWD to A> so other tests (VfsTest) are not affected
    try_chdir("A>")?;
    rd("B>fat32_test/dd")?;
    try_rmdir("B>fat32_test/dd")
}

fn test_large_file() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/large")?;
    let fd = try_open("B>fat32_test/large/big.bin", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    // Write 3 full clusters worth (assuming sec_per_clus >= 1, 512 bytes each)
    // Write byte values 0..255 three times = 768 bytes, still multi-cluster
    let mut buf = alloc::vec![0u8; 768];
    for i in 0..768 {
        buf[i] = (i & 0xFF) as u8;
    }
    try_write(fd, &buf)?;
    // Read back
    try_seek(fd, SeekFrom::Start(0))?;
    let mut read_buf = alloc::vec![0u8; 768];
    let n = try_read(fd, &mut read_buf)?;
    try_close(fd)?;
    if n != 768 {
        return Err("large file read returned wrong length");
    }
    // Spot-check
    if read_buf[0] != 0 { return Err("large file byte 0 mismatch"); }
    if read_buf[255] != 255 { return Err("large file byte 255 mismatch"); }
    if read_buf[256] != 0 { return Err("large file cross-cluster byte 256 mismatch"); }
    if read_buf[511] != 255 { return Err("large file byte 511 mismatch"); }
    if read_buf[512] != 0 { return Err("large file cross-cluster byte 512 mismatch"); }
    if read_buf[767] != 255 { return Err("large file byte 767 mismatch"); }
    rd("B>fat32_test/large")?;
    try_rmdir("B>fat32_test/large")
}

fn test_overwrite() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/over")?;
    let fd = try_open("B>fat32_test/over/data.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    try_write(fd, b"AAAABBBB")?;
    try_seek(fd, SeekFrom::Start(0))?;
    try_write(fd, b"CC")?;
    try_seek(fd, SeekFrom::Start(0))?;
    let mut buf = [0u8; 8];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 8 || &buf != b"CCAABBBB" {
        return Err("overwrite gave wrong content");
    }
    rd("B>fat32_test/over")?;
    try_rmdir("B>fat32_test/over")
}

fn test_long_filename() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/lfn")?;
    let name = "B>fat32_test/lfn/this-is-a-very-long-filename-example.txt";
    let fd = try_open(name, OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    try_write(fd, b"long name content")?;
    try_seek(fd, SeekFrom::Start(0))?;
    let mut buf = [0u8; 17];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 17 || &buf != b"long name content" {
        return Err("long filename file content mismatch");
    }
    let st = try_stat(name)?;
    if st.file_type != FileType::Regular {
        return Err("long filename stat wrong type");
    }
    if st.size != 17 {
        return Err("long filename stat wrong size");
    }
    rd("B>fat32_test/lfn")?;
    try_rmdir("B>fat32_test/lfn")
}

fn test_long_dirname() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/ldn")?;
    let dir = "B>fat32_test/ldn/a-directory-with-a-very-long-name";
    try_mkdir(dir)?;
    let st = try_stat(dir)?;
    if st.file_type != FileType::Directory {
        return Err("long dirname stat wrong type");
    }
    // File inside long-named dir
    let fpath = alloc::format!("{}/inner_file.txt", dir);
    let fd = try_open(&fpath, OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"inside")?;
    try_close(fd)?;
    let entries = try_readdir(dir)?;
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    if !names.contains(&"inner_file.txt") {
        return Err("readdir in long dir missing inner_file.txt");
    }
    rd("B>fat32_test/ldn")?;
    try_rmdir("B>fat32_test/ldn")
}

fn test_rename() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/rn")?;
    let fd = try_open("B>fat32_test/rn/old.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"rename me")?;
    try_close(fd)?;
    try_rename("B>fat32_test/rn/old.txt", "B>fat32_test/rn/new.txt")?;
    // New name exists
    try_stat("B>fat32_test/rn/new.txt")?;
    // Old name gone
    match vfs::stat("B>fat32_test/rn/old.txt") {
        Err(VfsError::NotFound) => {}
        _ => return Err("old path should not exist after rename"),
    }
    rd("B>fat32_test/rn")?;
    try_rmdir("B>fat32_test/rn")
}

fn test_open_errors() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/err")?;
    // Open non-existent without CREATE
    match vfs::open("B>fat32_test/err/nonexistent", OpenFlags::READ) {
        Err(VfsError::NotFound) => {}
        Ok(_) => return Err("open non-existent without CREATE should fail"),
        _ => return Err("wrong error for open non-existent"),
    }
    // CREATE on existing file -> AlreadyExists with EXCL
    let fd = try_open("B>fat32_test/err/existing.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd)?;
    match vfs::open("B>fat32_test/err/existing.txt", OpenFlags::CREATE | OpenFlags::EXCL | OpenFlags::WRITE) {
        Err(VfsError::AlreadyExists) => {}
        Ok(_) => return Err("CREATE+EXCL on existing should fail"),
        _ => return Err("wrong error for CREATE+EXCL"),
    }
    // mkdir on existing
    match vfs::mkdir("B>fat32_test/err") {
        Err(VfsError::AlreadyExists) => {}
        Ok(()) => return Err("mkdir existing dir should fail"),
        _ => {}
    }
    rd("B>fat32_test/err")?;
    try_rmdir("B>fat32_test/err")
}

fn test_write_to_dir() -> Result<(), &'static str> {
    try_mkdir("B>fat32_test/wtd")?;
    let fd = try_open("B>fat32_test/wtd", OpenFlags::READ)?;
    let result = vfs::write(fd, b"data");
    try_close(fd)?;
    match result {
        Err(VfsError::IsADirectory) => {}
        Ok(_) => return Err("write to dir should return IsADirectory"),
        _ => return Err("wrong error for write to dir"),
    }
    // Also test readdir on a file
    let fd2 = try_open("B>fat32_test/wtd/a_file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd2)?;
    match vfs::readdir("B>fat32_test/wtd/a_file.txt") {
        Err(VfsError::NotADirectory) => {}
        Ok(_) => return Err("readdir on file should return NotADirectory"),
        _ => return Err("wrong error for readdir on file"),
    }
    rd("B>fat32_test/wtd")?;
    try_rmdir("B>fat32_test/wtd")
}

fn test_statfs() -> Result<(), &'static str> {
    let st = vfs::statfs("B>").map_err(|_| "statfs failed")?;
    if st.block_size == 0 {
        return Err("statfs block_size is 0");
    }
    if st.total_blocks == 0 {
        return Err("statfs total_blocks is 0");
    }
    if st.free_blocks == 0 {
        return Err("statfs free_blocks is 0");
    }
    if st.free_blocks > st.total_blocks {
        return Err("statfs free_blocks > total_blocks");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Struct + Module impl
// ---------------------------------------------------------------------------

pub struct Fat32Test;

impl Module for Fat32Test {
    fn name(&self) -> &str {
        "fat32_test"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn init(&self, _display: &mut Framebuffer) -> Result<(), &'static str> {
        // Check if B> (ESP) is mounted — if not, skip gracefully.
        // Returning Ok(()) here is critical: registry.rs break-s on first
        // Err, which would prevent VfsTest from running.
        if vfs::stat("B>").is_err() {
            SerialPort::puts("[FAT32TEST] B> not mounted — skipping FAT32 tests\n");
            return Ok(());
        }

        SerialPort::puts("[FAT32TEST] === FAT32 Test Suite ===\n");

        // Clean up any leftovers from a previous crashed run
        cleanup_workdir();

        // Create the workdir
        if try_mkdir("B>fat32_test").is_err() {
            SerialPort::puts("[FAT32TEST] WARNING: could not create B>fat32_test\n");
            // Non-fatal — individual tests may fail but don't block VfsTest
        }

        t!("create_unlink",    test_create_unlink());
        t!("write_read",       test_write_read());
        t!("seek",             test_seek());
        t!("append",           test_append());
        t!("truncate",         test_truncate());
        t!("mkdir_rmdir",      test_mkdir_rmdir());
        t!("mkdir_nonempty",   test_mkdir_nonempty());
        t!("nested_dirs",      test_nested_dirs());
        t!("readdir",          test_readdir());
        t!("dotdot",           test_dotdot());
        t!("large_file",       test_large_file());
        t!("overwrite",        test_overwrite());
        t!("long_filename",    test_long_filename());
        t!("long_dirname",     test_long_dirname());
        t!("rename",           test_rename());
        t!("open_errors",      test_open_errors());
        t!("write_to_dir",     test_write_to_dir());
        t!("statfs",           test_statfs());

        // Best-effort final cleanup
        cleanup_workdir();

        let p = PASS.load(Ordering::Relaxed);
        let f = FAIL.load(Ordering::Relaxed);
        let mut port = SerialPort::new();
        write!(port, "[FAT32TEST] done: {}/{} passed", p, p + f).ok();
        if f > 0 {
            write!(port, " ({} FAILED)\n", f).ok();
        } else {
            write!(port, "\n").ok();
        }

        // Always return Ok — failures are per-test metrics, not a module
        // init failure (which would skip VfsTest).
        Ok(())
    }
}
