//! /proc/self — current-task control tree.
//!
//! Synthetic VFS tree for the currently executing task (falls back to the
//! op files alone when no task is running, e.g. in the kernel idle loop).
//! Standard streams appear as `0` (stdin, read-only), `1` (stdout, write-only)
//! and `2` (stderr, write-only) — these are the task's `io:stream` nodes, so
//! reads serve unconsumed history and writes append (and echo to the console
//! for Serial-kind streams).  `:status` is functional; `:exit` terminates the
//! current task; `:ctl` (spawn/kill/join/bind) and `:bind` (narrow) are
//! structure-only this phase — real dispatch is wired in Phase 5.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};
use crate::proc::current_task;
use crate::proc::stream::StreamNode;

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 10;
const INO_STDIN: u64 = 100;
const INO_STDOUT: u64 = 101;
const INO_STDERR: u64 = 102;
const INO_STATUS: u64 = 100000;
const INO_CTL: u64 = 100001;
const INO_BIND: u64 = 100002;
const INO_EXIT: u64 = 100003;

static SELF_OPS: [OpDesc; 4] = [
    OpDesc {
        name: ":status",
        rights: RightsMask::R,
        doc: "Read the current task's identity and stream backlog.",
    },
    OpDesc {
        name: ":ctl",
        rights: RightsMask::W,
        doc: "Task control: spawn/kill/join/bind (write request body).",
    },
    OpDesc {
        name: ":bind",
        rights: RightsMask::W,
        doc: "Bind/narrow a namespace path (write request body).",
    },
    OpDesc {
        name: ":exit",
        rights: RightsMask::W,
        doc: "Terminate the current task.",
    },
];

// ── Stream file ──────────────────────────────────────────────────────────

/// `/proc/self/N` — one of the task's standard streams.  Positional (offset
/// is ignored): `read` serves up to `buf.len()` unconsumed bytes advancing
/// the stream cursor; `write` appends to the stream history.
pub struct StreamFile {
    ino: u64,
    stream: Arc<StreamNode>,
    read_allowed: bool,
    write_allowed: bool,
}

impl FileOps for StreamFile {
    fn file_kind(&self) -> FileKind {
        FileKind::Device
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn read(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if !self.read_allowed {
            return Err(VfsError::NotSupported);
        }
        let data = self.stream.read(buf.len());
        let n = data.len().min(buf.len());
        if n > 0 {
            buf[..n].copy_from_slice(&data[..n]);
        }
        Ok(n)
    }

    fn write(&self, _offset: u64, data: &[u8]) -> Result<usize, VfsError> {
        if !self.write_allowed {
            return Err(VfsError::NotSupported);
        }
        self.stream.write(data);
        Ok(data.len())
    }

    fn size(&self) -> u64 {
        self.stream.buffered() as u64
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat {
            ino: self.ino,
            size: self.stream.buffered() as u64,
            file_kind: FileKind::Device,
            mtime: 0,
        })
    }
}

// ── Op nodes ─────────────────────────────────────────────────────────────

/// Functional `:status` op.
pub struct SelfStatus {
    ino: u64,
}

impl FileOps for SelfStatus {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let Some(task) = current_task() else {
            return Ok(0);
        };
        let text = format!(
            "id={}\npriority={}\nstdin={} buffered\nstdout={} buffered\nstderr={} buffered\n",
            task.id,
            task.priority,
            task.stdin.buffered(),
            task.stdout.buffered(),
            task.stderr.buffered(),
        );
        Ok(super::serve_text(text.as_bytes(), offset, buf))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }

    fn ops(&self) -> &'static [OpDesc] {
        &SELF_OPS
    }
}

/// Control op node: `:ctl` / `:bind` are structure-only (real dispatch wired
/// in Phase 5); `:exit` terminates the current task.
pub struct SelfOp {
    ino: u64,
    op: &'static str,
}

impl FileOps for SelfOp {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn write(&self, _offset: u64, data: &[u8]) -> Result<usize, VfsError> {
        match self.op {
            ":exit" => {
                let code = match data.first() {
                    Some(&b) => b as i64,
                    None => 0,
                };
                crate::proc::exit_process(code)
            }
            _ => Err(VfsError::NotSupported),
        }
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }

    fn ops(&self) -> &'static [OpDesc] {
        &SELF_OPS
    }
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /proc/self — current-task control tree.
pub struct ProcSelfRoot;

impl ProcSelfRoot {
    fn stream_entry(
        &self,
        ino: u64,
        role: fn(&crate::proc::Task) -> Arc<StreamNode>,
        read_allowed: bool,
        write_allowed: bool,
    ) -> Result<Arc<dyn FileOps>, VfsError> {
        let task = current_task().ok_or(VfsError::NotFound)?;
        Ok(Arc::new(StreamFile {
            ino,
            stream: role(&task),
            read_allowed,
            write_allowed,
        }))
    }
}

impl FileOps for ProcSelfRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let mut entries = Vec::new();
        if current_task().is_some() {
            entries.push(DirEntry { ino: INO_STDIN, name: String::from("0"), file_kind: FileKind::Device, rights: RightsMask::R });
            entries.push(DirEntry { ino: INO_STDOUT, name: String::from("1"), file_kind: FileKind::Device, rights: RightsMask::W });
            entries.push(DirEntry { ino: INO_STDERR, name: String::from("2"), file_kind: FileKind::Device, rights: RightsMask::W });
        }
        entries.push(DirEntry { ino: INO_STATUS, name: String::from(":status"), file_kind: FileKind::Op, rights: RightsMask::R });
        entries.push(DirEntry { ino: INO_CTL, name: String::from(":ctl"), file_kind: FileKind::Op, rights: RightsMask::W });
        entries.push(DirEntry { ino: INO_BIND, name: String::from(":bind"), file_kind: FileKind::Op, rights: RightsMask::W });
        entries.push(DirEntry { ino: INO_EXIT, name: String::from(":exit"), file_kind: FileKind::Op, rights: RightsMask::W });
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            "0" => self.stream_entry(INO_STDIN, |t| t.stdin.clone(), true, false),
            "1" => self.stream_entry(INO_STDOUT, |t| t.stdout.clone(), false, true),
            "2" => self.stream_entry(INO_STDERR, |t| t.stderr.clone(), false, true),
            ":status" => Ok(Arc::new(SelfStatus { ino: INO_STATUS })),
            ":ctl" => Ok(Arc::new(SelfOp { ino: INO_CTL, op: ":ctl" })),
            ":bind" => Ok(Arc::new(SelfOp { ino: INO_BIND, op: ":bind" })),
            ":exit" => Ok(Arc::new(SelfOp { ino: INO_EXIT, op: ":exit" })),
            _ => Err(VfsError::NotFound),
        }
    }

    fn ops(&self) -> &'static [OpDesc] {
        &SELF_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_ROOT, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the canonical `/proc/self` root.
pub fn proc_self() -> Arc<dyn FileOps> {
    Arc::new(ProcSelfRoot)
}
