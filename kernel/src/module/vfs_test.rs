use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::display::framebuffer::Framebuffer;
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
            write!(port, "[VFSTEST] {:35} ", $name).ok();
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

fn try_mount_virtual(source: &str, drive: char) -> Result<(), &'static str> {
    vfs::mount_virtual(source, drive).map_err(|_| "mount_virtual failed")
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

fn test_mkdir() -> Result<(), &'static str> {
    try_mkdir("A>test_mkdir")?;
    let st = try_stat("A>test_mkdir")?;
    if st.file_type != FileType::Directory {
        return Err("stat returned non-directory");
    }
    try_rmdir("A>test_mkdir")
}

fn test_open_create() -> Result<(), &'static str> {
    let fd = try_open("A>test_oc.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd)?;
    let st = try_stat("A>test_oc.txt")?;
    if st.file_type != FileType::Regular {
        return Err("stat returned non-regular");
    }
    try_unlink("A>test_oc.txt")
}

fn test_write_read() -> Result<(), &'static str> {
    let fd = try_open("A>test_wr.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    try_write(fd, b"HelloWorld")?;
    try_seek(fd, SeekFrom::Start(0))?;
    let mut buf = [0u8; 10];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 10 || &buf[..5] != b"Hello" {
        return Err("content mismatch");
    }
    try_unlink("A>test_wr.txt")
}

fn test_seek() -> Result<(), &'static str> {
    let fd = try_open("A>test_seek.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::READ)?;
    try_write(fd, b"0123456789")?;
    try_seek(fd, SeekFrom::Start(3))?;
    let mut buf = [0u8; 4];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 4 || &buf != b"3456" {
        return Err("seek+read gave wrong data");
    }
    try_unlink("A>test_seek.txt")
}

fn test_relative_path() -> Result<(), &'static str> {
    try_mkdir("A>test_rp")?;
    try_chdir("A>test_rp")?;
    let fd = try_open("rel_file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"relative")?;
    try_close(fd)?;
    // Verify via absolute path
    let st = try_stat("A>test_rp/rel_file.txt")?;
    if st.size != 8 {
        return Err("wrong size via absolute stat");
    }
    // Clean up
    try_unlink("A>test_rp/rel_file.txt")?;
    try_chdir("A>")?;
    try_rmdir("A>test_rp")
}

fn test_dotdot() -> Result<(), &'static str> {
    try_mkdir("A>test_dd")?;
    try_mkdir("A>test_dd/sub")?;
    try_chdir("A>test_dd/sub")?;
    let cwd = try_getcwd()?;
    if cwd != "A>test_dd/sub" {
        return Err("getcwd after chdir");
    }
    try_chdir("..")?;
    let cwd2 = try_getcwd()?;
    if cwd2 != "A>test_dd" {
        return Err("getcwd after ..");
    }
    try_chdir("..")?;
    let cwd3 = try_getcwd()?;
    if cwd3 != "A>" {
        return Err("getcwd after ../..");
    }
    // Clamped at root
    try_chdir("..")?;
    let cwd4 = try_getcwd()?;
    if cwd4 != "A>" {
        return Err("getcwd after ... (should clamp)");
    }
    // Cleanup
    try_chdir("A>")?;
    try_rmdir("A>test_dd/sub")?;
    try_rmdir("A>test_dd")
}

fn test_rename() -> Result<(), &'static str> {
    let fd = try_open("A>test_rn_old.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"rename me")?;
    try_close(fd)?;
    try_rename("A>test_rn_old.txt", "A>test_rn_new.txt")?;
    // New name exists
    try_stat("A>test_rn_new.txt")?;
    // Old name gone
    match vfs::stat("A>test_rn_old.txt") {
        Err(VfsError::NotFound) => {}
        _ => return Err("old path should not exist after rename"),
    }
    try_unlink("A>test_rn_new.txt")
}

fn test_unlink() -> Result<(), &'static str> {
    let fd = try_open("A>test_unlink_del.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd)?;
    try_unlink("A>test_unlink_del.txt")?;
    match vfs::stat("A>test_unlink_del.txt") {
        Err(VfsError::NotFound) => Ok(()),
        _ => Err("stat should return NotFound after unlink"),
    }
}

fn test_rmdir() -> Result<(), &'static str> {
    try_mkdir("A>test_rmdir_d")?;
    try_rmdir("A>test_rmdir_d")?;
    match vfs::stat("A>test_rmdir_d") {
        Err(VfsError::NotFound) => Ok(()),
        _ => Err("stat should return NotFound after rmdir"),
    }
}

fn test_rmdir_nonempty() -> Result<(), &'static str> {
    try_mkdir("A>test_ne")?;
    let fd = try_open("A>test_ne/f", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(fd)?;
    match vfs::rmdir("A>test_ne") {
        Err(VfsError::NotEmpty) => {}
        Ok(()) => return Err("rmdir on non-empty should fail"),
        _ => return Err("wrong error for rmdir non-empty"),
    }
    try_unlink("A>test_ne/f")?;
    try_rmdir("A>test_ne")
}

fn test_truncate() -> Result<(), &'static str> {
    let fd = try_open("A>test_trunc.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"HelloWorld")?;
    try_close(fd)?;
    try_truncate("A>test_trunc.txt", 0)?;
    let st = try_stat("A>test_trunc.txt")?;
    if st.size != 0 {
        return Err("size not 0 after truncate");
    }
    let fd = try_open("A>test_trunc.txt", OpenFlags::READ)?;
    let mut buf = [0u8; 4];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 0 {
        return Err("should read 0 bytes after truncate");
    }
    try_unlink("A>test_trunc.txt")
}

fn test_append() -> Result<(), &'static str> {
    let fd = try_open("A>test_append.txt", OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::READ)?;
    try_write(fd, b"Hello")?;
    try_write(fd, b"World")?;
    try_seek(fd, SeekFrom::Start(0))?;
    let mut buf = [0u8; 10];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 10 || &buf != b"HelloWorld" {
        return Err("APPEND content mismatch");
    }
    try_unlink("A>test_append.txt")
}

fn test_readdir() -> Result<(), &'static str> {
    try_mkdir("A>test_rd_dir")?;
    let f1 = try_open("A>test_rd_dir/a.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(f1)?;
    let f2 = try_open("A>test_rd_dir/b.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_close(f2)?;
    let entries = try_readdir("A>test_rd_dir")?;
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    if !names.contains(&".") {
        return Err("readdir missing .");
    }
    if !names.contains(&"..") {
        return Err("readdir missing ..");
    }
    if !names.contains(&"a.txt") {
        return Err("readdir missing a.txt");
    }
    if !names.contains(&"b.txt") {
        return Err("readdir missing b.txt");
    }
    try_unlink("A>test_rd_dir/a.txt")?;
    try_unlink("A>test_rd_dir/b.txt")?;
    try_rmdir("A>test_rd_dir")
}

fn test_open_errors() -> Result<(), &'static str> {
    match vfs::open("A>nonexistent_file_xyz", OpenFlags::READ) {
        Err(VfsError::NotFound) => {}
        Ok(_) => return Err("open non-existent without CREATE should fail"),
        _ => return Err("wrong error for open non-existent"),
    }
    match vfs::mkdir("A>") {
        Err(VfsError::AlreadyExists) => {}
        Ok(()) => return Err("mkdir / should fail"),
        _ => {}
    }
    Ok(())
}

fn test_dot_slash() -> Result<(), &'static str> {
    try_mkdir("A>test_dot")?;
    let fd = try_open("A>test_dot/././file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"dots")?;
    try_close(fd)?;
    let fd = try_open("A>test_dot/file.txt", OpenFlags::READ)?;
    let mut buf = [0u8; 4];
    let n = try_read(fd, &mut buf)?;
    try_close(fd)?;
    if n != 4 || &buf != b"dots" {
        return Err("./ path gave wrong content");
    }
    try_unlink("A>test_dot/file.txt")?;
    try_rmdir("A>test_dot")
}

fn test_mount_virtual() -> Result<(), &'static str> {
    try_mkdir("A>test_mv_src")?;
    try_mount_virtual("A>test_mv_src", 'D')?;
    let fd = try_open("D>virt_file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"virtual")?;
    try_close(fd)?;
    // Verify it appears on source
    let st = try_stat("A>test_mv_src/virt_file.txt")?;
    if st.size != 7 {
        return Err("virtual file size mismatch");
    }
    // Cleanup
    try_unlink("D>virt_file.txt")?;
    vfs::unmount('D').map_err(|_| "unmount failed")?;
    try_rmdir("A>test_mv_src")
}

fn test_stat() -> Result<(), &'static str> {
    try_mkdir("A>test_stat_dir")?;
    let st = try_stat("A>test_stat_dir")?;
    if st.file_type != FileType::Directory {
        return Err("stat on dir returned wrong type");
    }
    if st.ino == 0 {
        return Err("stat ino is 0");
    }
    try_rmdir("A>test_stat_dir")?;
    let fd = try_open("A>test_stat_file.txt", OpenFlags::CREATE | OpenFlags::WRITE)?;
    try_write(fd, b"12345")?;
    try_close(fd)?;
    let st = try_stat("A>test_stat_file.txt")?;
    if st.file_type != FileType::Regular {
        return Err("stat on file returned wrong type");
    }
    if st.size != 5 {
        return Err("stat size wrong");
    }
    try_unlink("A>test_stat_file.txt")
}

// ---------------------------------------------------------------------------
// Struct + Module impl
// ---------------------------------------------------------------------------

pub struct VfsTest;

impl Module for VfsTest {
    fn name(&self) -> &str {
        "vfs_test"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn init(&self, _display: &mut Framebuffer) -> Result<(), &'static str> {
        SerialPort::puts("[VFSTEST] === VFS Test Suite ===\n");

        t!("mkdir", test_mkdir());
        t!("open_create", test_open_create());
        t!("write_read", test_write_read());
        t!("seek", test_seek());
        t!("relative_path", test_relative_path());
        t!("dotdot", test_dotdot());
        t!("rename", test_rename());
        t!("unlink", test_unlink());
        t!("rmdir", test_rmdir());
        t!("rmdir_nonempty", test_rmdir_nonempty());
        t!("truncate", test_truncate());
        t!("append", test_append());
        t!("readdir", test_readdir());
        t!("open_errors", test_open_errors());
        t!("dot_slash", test_dot_slash());
        t!("mount_virtual", test_mount_virtual());
        t!("stat", test_stat());

        let p = PASS.load(Ordering::Relaxed);
        let f = FAIL.load(Ordering::Relaxed);
        let mut port = SerialPort::new();
        write!(port, "[VFSTEST] done: {}/{} passed", p, p + f).ok();
        if f > 0 {
            write!(port, " ({} FAILED)\n", f).ok();
        } else {
            write!(port, "\n").ok();
        }

        if f > 0 {
            Err("VFS tests failed")
        } else {
            Ok(())
        }
    }
}
