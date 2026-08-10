//! /tasks — per-task trees.
//!
//! Synthetic VFS tree over the scheduler's append-only task registry.  Each
//! task appears as `/tasks/<id>` exposing a functional `:status` op plus
//! structure-only `:kill` / `:join` ops.  Tasks are never freed while a
//! snapshot Arc is held, so the tree can never dangle.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::error::VfsError;
use super::super::file_ops::{FileOps, OpDesc};
use super::super::types::{DirEntry, FileKind, RightsMask, Stat};
use crate::proc::{task_by_id, task_snapshot, Task, TASK_RUNNABLE, TASK_SLEEPING, TASK_ZOMBIE};

// ── Inode map ────────────────────────────────────────────────────────────
const INO_ROOT: u64 = 20;
const INO_BASE: u64 = 20000;
const INO_STATUS: u64 = 100000;
const INO_KILL: u64 = 100001;
const INO_JOIN: u64 = 100002;

static TASK_OPS: [OpDesc; 3] = [
    OpDesc {
        name: ":status",
        rights: RightsMask::R,
        doc: "Read the task's id, priority, scheduler state, and stream backlog.",
    },
    OpDesc {
        name: ":kill",
        rights: RightsMask::W,
        doc: "Request the task be torn down (write request body).",
    },
    OpDesc {
        name: ":join",
        rights: RightsMask::W,
        doc: "Wait for the task to exit (write request body).",
    },
];

fn state_label(state: u8) -> &'static str {
    if state == TASK_RUNNABLE {
        "runnable"
    } else if state == TASK_SLEEPING {
        "sleeping"
    } else if state == TASK_ZOMBIE {
        "zombie"
    } else {
        "unknown"
    }
}

// ── Op nodes ─────────────────────────────────────────────────────────────

/// Functional `:status` op.
pub struct TaskStatus {
    ino: u64,
    task: Arc<Task>,
}

impl FileOps for TaskStatus {
    fn file_kind(&self) -> FileKind {
        FileKind::Op
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let text = format!(
            "id={}\npriority={}\nstate={}\nstdin_buffered={}\nstdout_buffered={}\nstderr_buffered={}\n",
            self.task.id,
            self.task.priority,
            state_label(self.task.state()),
            self.task.stdin.buffered(),
            self.task.stdout.buffered(),
            self.task.stderr.buffered(),
        );
        Ok(super::serve_text(text.as_bytes(), offset, buf))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino, size: 0, file_kind: FileKind::Op, mtime: 0 })
    }
}

/// Structure-only `:kill` / `:join` op (real teardown/join dispatch is wired
/// in Phase 5).
pub struct TaskOp {
    ino: u64,
}

impl FileOps for TaskOp {
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

// ── Per-task directory ───────────────────────────────────────────────────

/// `/tasks/<id>` — one task.
pub struct TaskEntry {
    task: Arc<Task>,
}

impl FileOps for TaskEntry {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_BASE + self.task.id as u64
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        Ok(alloc::vec![
            DirEntry { ino: INO_STATUS, name: String::from(":status"), file_kind: FileKind::Op, rights: RightsMask::R },
            DirEntry { ino: INO_KILL, name: String::from(":kill"), file_kind: FileKind::Op, rights: RightsMask::W },
            DirEntry { ino: INO_JOIN, name: String::from(":join"), file_kind: FileKind::Op, rights: RightsMask::W },
        ])
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        match name {
            ":status" => Ok(Arc::new(TaskStatus { ino: INO_STATUS, task: self.task.clone() })),
            ":kill" => Ok(Arc::new(TaskOp { ino: INO_KILL })),
            ":join" => Ok(Arc::new(TaskOp { ino: INO_JOIN })),
            _ => Err(VfsError::NotFound),
        }
    }

    fn ops(&self) -> &'static [OpDesc] {
        &TASK_OPS
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: self.ino(), size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

// ── Root ─────────────────────────────────────────────────────────────────

/// /tasks — per-task trees.
pub struct TasksRoot;

impl FileOps for TasksRoot {
    fn file_kind(&self) -> FileKind {
        FileKind::Directory
    }

    fn ino(&self) -> u64 {
        INO_ROOT
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, VfsError> {
        let tasks = task_snapshot();
        let mut entries = Vec::with_capacity(tasks.len());
        for task in tasks.iter() {
            entries.push(DirEntry {
                ino: INO_BASE + task.id as u64,
                name: task.id.to_string(),
                file_kind: FileKind::Directory,
                rights: RightsMask::R,
            });
        }
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileOps>, VfsError> {
        let id: u32 = name.parse().map_err(|_| VfsError::NotFound)?;
        let task = task_by_id(id).ok_or(VfsError::NotFound)?;
        Ok(Arc::new(TaskEntry { task }))
    }

    fn getattr(&self) -> Result<Stat, VfsError> {
        Ok(Stat { ino: INO_ROOT, size: 0, file_kind: FileKind::Directory, mtime: 0 })
    }
}

/// Construct the canonical `/tasks` root.
pub fn tasks_root() -> Arc<dyn FileOps> {
    Arc::new(TasksRoot)
}
