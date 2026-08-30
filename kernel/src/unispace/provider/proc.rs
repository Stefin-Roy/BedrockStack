//! `/proc` provider — the process pseudo-filesystem (x86_64 only).
//!
//! `/proc/<pid>` is a directory per live task: tasks are `attach`ed here at
//! spawn/mount into the scheduler and `detach`ed at reap, so a clock tick
//! needs no registry of its own. `/proc/self` aliases the caller (and is
//! absent in kernel context, where no task is running).
//!
//! Each `<pid>` dir exposes:
//!   - `status` — `read` yields `{pid, state, exit_code}` from the scheduler
//!     snapshot (`exit_code` is retained while the task is a zombie);
//!   - `mem`    — `read` yields the eager user-memory accounting
//!     `{root, brk, stack_top, committed_pages, budget_pages}` from `mm::usermem`;
//!   - `args`   — `read` yields the caller's `:spawn` argument string;
//!   - `std`    — the task's standard streams: `std/in` is an append-only
//!     input leaf, and `std/out`/`std/err` drain on `read` (pipe/monitor
//!     semantics, capped at 64 KiB with the oldest bytes dropped so a chatty
//!     task cannot exhaust the heap); their `:get` method blocks the calling
//!     task until output is available (x86_64 only);
//!   - `:exit`  — write `{code}` to terminate the current task (diverges);
//!   - `:kill`  — write `{pid}` to park another task for reaping;
//!   - `:spawn` — write `{path, args}` to fork a new process from an ELF on a
//!     mounted filesystem;
//!   - `:wait`  — write `{pid}` to block until a child exits and receive its
//!     `{code}` (children only, Unix wait() semantics);
//!   - `:brk`   — write `{new_break}` to grow/shrink/query the caller's break;
//!   - `:mmap`  — write `{addr, len, prot}` to eagerly commit anonymous pages;
//!   - `:munmap`— write `{addr, len}` to release whole anonymous regions.
//!
//! `:brk`/`:mmap`/`:munmap` target the address space of the *running* task
//! (they mutate the caller's CR3), so they are meaningful only on `/proc/self`
//! regardless of which pid the path names.
//!
//! `ProcDir` stores only its `pid` and the three standard-stream handles —
//! never a `&'static mut Task`. Dead tasks
//! park as zombies and keep their `/proc` dir (so a parent can find and
//! `:wait` them) until `reap_dead`/a consuming `:wait` detaches them, so a
//! stale entry can never outlive its task.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hashbrown::HashMap;
use spin::{Mutex, Once};

use super::super::schema::{self, EnumVariant, MethodDesc, Schema, Value};
use super::super::{ListingEntry, Object, ObjectKind, UnispaceError};

// ── Schemas ──────────────────────────────────────────────────────────

/// Scheduler task states, in wire order (matches `TaskState`).
static PROC_STATE_VARIANTS: [EnumVariant; 4] = [
    EnumVariant {
        name: "ready",
        value: 0,
    },
    EnumVariant {
        name: "running",
        value: 1,
    },
    EnumVariant {
        name: "zzz",
        value: 2,
    },
    EnumVariant {
        name: "dead",
        value: 3,
    },
];

/// `read(/proc/<pid>/status)`: `struct{ pid: u64, state: enum, exit_code: u64, ppid: u64 }`.
/// `exit_code` is the retained code of a zombie (0 for a live task).
static STATUS: Schema = Schema::Struct(&[
    schema::Field {
        name: "pid",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "state",
        ty: &Schema::Enum(&PROC_STATE_VARIANTS),
    },
    schema::Field {
        name: "exit_code",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "ppid",
        ty: &schema::SCHEMA_U64,
    },
]);

/// `read(/proc/<pid>/mem)`: eager user-memory snapshot from `mm::usermem`.
static MEM: Schema = Schema::Struct(&[
    schema::Field {
        name: "root",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "brk",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "stack_top",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "committed_pages",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "budget_pages",
        ty: &schema::SCHEMA_U64,
    },
]);

/// `write(/proc/<pid>:exit, { code })`.
static EXIT_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "code",
    ty: &schema::SCHEMA_U64,
}]);

/// `write(/proc/<pid>:kill, { pid })`.
static KILL_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "pid",
    ty: &schema::SCHEMA_U64,
}]);

/// `write(/proc/<pid>:spawn_caps, { path, args, caps })` — replaces legacy `spawn`.
static CAP_ENTRY: Schema = Schema::Struct(&[
    schema::Field {
        name: "path",
        ty: &schema::SCHEMA_STR,
    },
    schema::Field {
        name: "method",
        ty: &schema::SCHEMA_STR,
    },
    schema::Field {
        name: "perm",
        ty: &schema::SCHEMA_U32,
    },
]);
static CAP_LIST: Schema = Schema::List(&CAP_ENTRY);
static SPAWN_CAPS_INPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "path",
        ty: &schema::SCHEMA_STR,
    },
    schema::Field {
        name: "args",
        ty: &schema::SCHEMA_STR,
    },
    schema::Field {
        name: "caps",
        ty: &CAP_LIST,
    },
]);

/// `spawn` output: the new task's pid.
static SPAWN_OUTPUT: Schema = Schema::Struct(&[schema::Field {
    name: "pid",
    ty: &schema::SCHEMA_U64,
}]);

/// `write(/proc/self:brk, { new_break })` — grow/shrink/query the caller's
/// committed program break. `{new_break: 0}` (or below the floor) is a query
/// returning the current break.
static BRK_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "new_break",
    ty: &schema::SCHEMA_U64,
}]);

/// `brk` output: the resulting break.
static BRK_OUTPUT: Schema = Schema::Struct(&[schema::Field {
    name: "brk",
    ty: &schema::SCHEMA_U64,
}]);

/// `write(/proc/self:mmap, { addr, len, prot })` — eagerly commit `len` zeroed
/// anonymous pages. `{addr: 0}` picks the first free gap above the break.
static MMAP_INPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "addr",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "len",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "prot",
        ty: &schema::SCHEMA_U64,
    },
]);

/// `mmap` output: the mapping base.
static MMAP_OUTPUT: Schema = Schema::Struct(&[schema::Field {
    name: "base",
    ty: &schema::SCHEMA_U64,
}]);

/// `write(/proc/<pid>:munmap, { addr, len })` — release whole anonymous regions.
static MUNMAP_INPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "addr",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "len",
        ty: &schema::SCHEMA_U64,
    },
]);

/// `write(/proc/<pid>:wait, { pid })` — block until the target child exits and
/// return its exit code.
static WAIT_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "pid",
    ty: &schema::SCHEMA_U64,
}]);

/// `write(/proc/self:pkey_mprotect, { addr, len, key })` — tag a whole
/// anon/heap region with PKU protection key `key` (0 clears).
static PKEY_MPROTECT_INPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "addr",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "len",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "key",
        ty: &schema::SCHEMA_U64,
    },
]);

/// `write(/proc/self:pkey_set, { key, rights })` — set this task's PKRU
/// rights for one key: 0 = read/write, 1 = read-only (WD), 2 = no access (AD).
static PKEY_SET_INPUT: Schema = Schema::Struct(&[
    schema::Field {
        name: "key",
        ty: &schema::SCHEMA_U64,
    },
    schema::Field {
        name: "rights",
        ty: &schema::SCHEMA_U64,
    },
]);

/// `wait` output: the consumed exit code.
static WAIT_OUTPUT: Schema = Schema::Struct(&[schema::Field {
    name: "code",
    ty: &schema::SCHEMA_U64,
}]);

static PROC_METHODS: [MethodDesc; 10] = [
    MethodDesc {
        name: "exit",
        input: &EXIT_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
    MethodDesc {
        name: "kill",
        input: &KILL_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
    MethodDesc {
        name: "spawn_caps",
        input: &SPAWN_CAPS_INPUT,
        output: &SPAWN_OUTPUT,
    },
    MethodDesc {
        name: "brk",
        input: &BRK_INPUT,
        output: &BRK_OUTPUT,
    },
    MethodDesc {
        name: "mmap",
        input: &MMAP_INPUT,
        output: &MMAP_OUTPUT,
    },
    MethodDesc {
        name: "munmap",
        input: &MUNMAP_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
    MethodDesc {
        name: "wait",
        input: &WAIT_INPUT,
        output: &WAIT_OUTPUT,
    },
    MethodDesc {
        name: "fork",
        input: &schema::SCHEMA_UNIT,
        output: &SPAWN_OUTPUT,
    },
    MethodDesc {
        name: "pkey_mprotect",
        input: &PKEY_MPROTECT_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
    MethodDesc {
        name: "pkey_set",
        input: &PKEY_SET_INPUT,
        output: &schema::SCHEMA_UNIT,
    },
];

// ── Registry ─────────────────────────────────────────────────────────

static PROCS: Once<Mutex<HashMap<String, Arc<ProcDir>>>> = Once::new();

/// The live-pid registry, created on first use (`HashMap::new` is not const).
fn procs() -> &'static Mutex<HashMap<String, Arc<ProcDir>>> {
    PROCS.call_once(|| Mutex::new(HashMap::new()))
}

fn pid_key(pid: u64) -> String {
    alloc::format!("{}", pid)
}

/// Register a live task's directory. Idempotent (a re-registration replaces
/// the previous entry with a fresh `ProcDir` of the same pid).
pub fn attach(pid: u64) {
    procs().lock().insert(
        pid_key(pid),
        Arc::new(ProcDir {
            pid,
            stdin: StdStream::new(),
            stdout: StdStream::new(),
            stderr: StdStream::new(),
        }),
    );
}

/// Deregister a task's directory (called from `reap_dead` before the task
/// box is dropped).
pub fn detach(pid: u64) {
    procs().lock().remove(&pid_key(pid));
}

/// The running task's pid, or `None` in kernel context (current_task null).
fn current_pid() -> Option<u64> {
    let pc = crate::smp::current_per_cpu();
    let ptr = pc.current_task.load(core::sync::atomic::Ordering::Relaxed);
    if ptr.is_null() {
        return None;
    }
    let t = unsafe { &*(ptr as *const crate::task::Task) };
    Some(t.id)
}

/// Register the `/proc` system (x86_64 only).
pub fn register() -> Result<(), UnispaceError> {
    super::super::register("proc", Arc::new(ProcRoot))
}

// ── /proc root ───────────────────────────────────────────────────────

/// `/proc`: a custom directory aliasing `self` to the caller and listing the
/// live pid dirs.
struct ProcRoot;

impl Object for ProcRoot {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Dir
    }

    fn value_schema(&self) -> &'static Schema {
        &super::super::DIR_SCHEMA
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        if name == "self" {
            let cur = current_pid()?;
            return procs()
                .lock()
                .get(&pid_key(cur))
                .cloned()
                .map(|p| p as Arc<dyn Object>);
        }
        procs()
            .lock()
            .get(name)
            .cloned()
            .map(|p| p as Arc<dyn Object>)
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        let guard = procs().lock();
        let mut names: Vec<String> = guard.keys().cloned().collect();
        names.sort();
        for n in names {
            out.push(ListingEntry {
                name: n,
                kind: ObjectKind::Dir,
            });
        }
        if current_pid().is_some() {
            out.push(ListingEntry {
                name: String::from("self"),
                kind: ObjectKind::Dir,
            });
        }
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut entries = Vec::new();
        self.list(&mut entries)?;
        super::super::encode_listing(entries, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }

    fn invoke(&self, _method: usize, _v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        Err(UnispaceError::MethodNotFound)
    }
}

// ── /proc/<pid> ──────────────────────────────────────────────────────

/// One live task's directory. Holds only the pid — the `Task` box lives in
/// the scheduler and is freed at reap (after `detach`).
struct ProcDir {
    pid: u64,
    stdin: StdStream,
    stdout: StdStream,
    stderr: StdStream,
}

impl Object for ProcDir {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Dir
    }

    fn value_schema(&self) -> &'static Schema {
        &super::super::DIR_SCHEMA
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &PROC_METHODS
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        if name == "status" {
            return Some(Arc::new(StatusObject { pid: self.pid }) as Arc<dyn Object>);
        }
        if name == "mem" {
            return Some(Arc::new(MemObject { pid: self.pid }) as Arc<dyn Object>);
        }
        if name == "args" {
            return Some(Arc::new(ArgsObject { pid: self.pid }) as Arc<dyn Object>);
        }
        if name == "caps" {
            return Some(Arc::new(CapsObject { pid: self.pid }) as Arc<dyn Object>);
        }
        if name == "std" {
            return Some(Arc::new(StdDir::of(self)) as Arc<dyn Object>);
        }
        None
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        out.push(ListingEntry {
            name: String::from("status"),
            kind: ObjectKind::Service,
        });
        out.push(ListingEntry {
            name: String::from("mem"),
            kind: ObjectKind::Service,
        });
        out.push(ListingEntry {
            name: String::from("args"),
            kind: ObjectKind::Service,
        });
        out.push(ListingEntry {
            name: String::from("caps"),
            kind: ObjectKind::Service,
        });
        out.push(ListingEntry {
            name: String::from("std"),
            kind: ObjectKind::Dir,
        });
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut entries = Vec::new();
        self.list(&mut entries)?;
        super::super::encode_listing(entries, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }

    fn invoke(&self, method: usize, v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                // exit: permanently abandons this syscall activation.
                let code = arg_u64(&v, 0)?;
                crate::task::exit_current(code)
            }
            1 => {
                let target = arg_u64(&v, 0)?;
                crate::task::kill(target).map_err(|_| UnispaceError::NotFound)
            }
            2 => {
                let path = arg_str(&v, 0)?;
                let args = arg_str(&v, 1)?;
                let caps = arg_caps(&v, 2)?;
                spawn_proc(path, args, caps, out)
            }
            3 => {
                // brk: grow/shrink/query the *current* task's break. Must run
                // on the caller's CR3 — never self.pid's address space.
                let new_break = arg_u64(&v, 0)?;
                let b = mem_method(|vm, alloc| crate::mm::usermem::brk(vm, new_break, alloc))?;
                let v = Value::Struct(vec![Value::U64(b)]);
                schema::encode_value(&v, &BRK_OUTPUT, out)
            }
            4 => {
                let addr = arg_u64(&v, 0)?;
                let len = arg_u64(&v, 1)?;
                let prot = arg_u64(&v, 2)?;
                let base =
                    mem_method(|vm, alloc| crate::mm::usermem::mmap(vm, addr, len, prot, alloc))?;
                let v = Value::Struct(vec![Value::U64(base)]);
                schema::encode_value(&v, &MMAP_OUTPUT, out)
            }
            5 => {
                let addr = arg_u64(&v, 0)?;
                let len = arg_u64(&v, 1)?;
                mem_method(|vm, alloc| crate::mm::usermem::munmap(vm, addr, len, alloc))?;
                Ok(())
            }
            6 => {
                // wait: block until a *child* of the caller exits and consume
                // its exit code.  Mirrors :kill — the path's pid is ignored;
                // the target is named in the payload.
                let target = arg_u64(&v, 0)?;
                let code = crate::task::wait(target).map_err(|e| match e {
                    crate::task::WaitError::NotChild => UnispaceError::InvalidArgument,
                    crate::task::WaitError::NotFound => UnispaceError::NotFound,
                })?;
                let v = Value::Struct(vec![Value::U64(code)]);
                schema::encode_value(&v, &WAIT_OUTPUT, out)
            }
            7 => {
                // fork: COW-clone the caller. The child resumes inside its
                // own syscall return with rax=0; the parent gets {pid}.
                let pid = crate::task::fork_current().map_err(|e| match e {
                    -12 => UnispaceError::OutOfMemory,
                    -22 => UnispaceError::InvalidArgument,
                    _ => UnispaceError::Unsupported,
                })?;
                let v = Value::Struct(vec![Value::U64(pid)]);
                schema::encode_value(&v, &SPAWN_OUTPUT, out)
            }
            8 => {
                // pkey_mprotect: tag a whole anon/heap region with a PKU key.
                let addr = arg_u64(&v, 0)?;
                let len = arg_u64(&v, 1)?;
                let key = arg_u64(&v, 2)?;
                if key > 15 {
                    return Err(UnispaceError::InvalidArgument);
                }
                mem_method(|vm, _alloc| crate::mm::usermem::pkey_protect(vm, addr, len, key as u8))?;
                Ok(())
            }
            9 => {
                // pkey_set: adjust this task's PKRU rights for one key.
                // 0 = RW, 1 = read-only (WD), 2 = no access (AD).
                let key = arg_u64(&v, 0)?;
                let rights = arg_u64(&v, 1)?;
                if key > 15 || rights > 2 {
                    return Err(UnispaceError::InvalidArgument);
                }
                set_task_pkru(key as u8, rights as u8)
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}

// ── /proc/<pid>/status ───────────────────────────────────────────────

/// Service leaf: `read` snapshots the scheduler state of `self.pid`.
struct StatusObject {
    pid: u64,
}

impl Object for StatusObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &STATUS
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let state = match crate::task::process_state(self.pid) {
            Some(s) => s,
            None => return Err(UnispaceError::NotFound),
        };
        let disc = match state {
            crate::task::TaskState::Ready => 0,
            crate::task::TaskState::Running => 1,
            crate::task::TaskState::ZzZ => 2,
            crate::task::TaskState::Dead => 3,
        };
        let exit_code = crate::task::task_exit_code(self.pid).unwrap_or(0);
        let ppid = crate::task::task_parent_pid(self.pid).unwrap_or(0);
        let v = Value::Struct(vec![
            Value::U64(self.pid),
            Value::Enum(disc),
            Value::U64(exit_code),
            Value::U64(ppid),
        ]);
        schema::encode_value(&v, &STATUS, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }
}

// ── /proc/<pid>/mem ─────────────────────────────────────────────────

/// Service leaf: `read` snapshots the eager user-memory accounting of
/// `self.pid` from `mm::usermem` (through the scheduler's pid→vm lookup).
struct MemObject {
    pid: u64,
}

impl Object for MemObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &MEM
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let vm = crate::task::task_vm(self.pid).ok_or(UnispaceError::NotFound)?;
        let s = crate::mm::usermem::summarize(vm).ok_or(UnispaceError::NotFound)?;
        let v = Value::Struct(vec![
            Value::U64(s.root),
            Value::U64(s.brk_cur),
            Value::U64(s.stack_top),
            Value::U64(s.committed_pages),
            Value::U64(s.budget_pages),
        ]);
        schema::encode_value(&v, &MEM, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }
}

// ── /proc/<pid>/args ───────────────────────────────────────────────

/// Service leaf: `read` yields the `:spawn` argument string of `self.pid`
/// (read-only).  The entry-point ABI never changes — a program fetches its own
/// arguments via `read(/proc/self/args)`.
struct ArgsObject {
    pid: u64,
}

impl Object for ArgsObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_STR
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let args = crate::task::task_args(self.pid).ok_or(UnispaceError::NotFound)?;
        schema::encode_value(&Value::Str(args), &schema::SCHEMA_STR, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }
}

// ── /proc/<pid>/caps ───────────────────────────────────────────────

/// Service leaf: `read` yields the task's capability set.
/// Value is `list<{path: str, method: str, perm: u32}>` matching `CAP_LIST` wire.
/// `method == ""` encodes `None`. Read requires `R` on `proc/self/caps` (ancestor `R` on `proc`/`proc/self`).
struct CapsObject {
    pid: u64,
}

impl Object for CapsObject {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Service
    }

    fn value_schema(&self) -> &'static Schema {
        &CAP_LIST
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        // `caps_snapshot` returns Some(empty) for user tasks with no caps, None for unknown pid.
        // Kernel threads (vm==0) with bypass would have returned None; treat as NotFound.
        let caps = crate::task::caps_snapshot(self.pid).ok_or(UnispaceError::NotFound)?;
        let mut items = Vec::with_capacity(caps.len());
        for c in caps {
            let method_str = c.method.unwrap_or_default();
            let perm_u32 = c.perm as u32;
            items.push(Value::Struct(vec![
                Value::Str(c.path),
                Value::Str(method_str),
                Value::U64(perm_u32 as u64),
            ]));
        }
        let v = Value::List(items);
        schema::encode_value(&v, &CAP_LIST, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }
}

// ── /proc/<pid>/std ────────────────────────────────────────────────

/// Bounded per-process byte stream exposed as a unispace object. The value is
/// a `Blob`; `read` drains up to `max` bytes (pipe/monitor semantics), `write`
/// appends. The buffer is capped at `STREAM_CAP` bytes and overflow drops the
/// OLDEST bytes (ring behavior) so one chatty process cannot exhaust the heap.
const STREAM_CAP: usize = 64 * 1024;

/// A cloneable handle to one of the task's standard streams (`in`/`out`/`err`).
/// The buffer lives behind an `Arc<Mutex<Vec<u8>>>`, so a `StdDir` exposes the
/// same live streams its `ProcDir` owns — writes through one handle are visible
/// to every reader, and `attach` creates one `StdStream` per pid.
#[derive(Clone)]
struct StdStream {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl StdStream {
    fn new() -> Self {
        StdStream {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Append, dropping the oldest bytes once the cap is exceeded.
    fn append(&self, bytes: &[u8]) {
        let mut b = self.buf.lock();
        b.extend_from_slice(bytes);
        let excess = b.len().saturating_sub(STREAM_CAP);
        if excess > 0 {
            b.drain(..excess);
        }
    }

    /// Remove and return up to `max` oldest bytes (a drain).
    fn drain(&self, max: usize) -> Vec<u8> {
        let mut b = self.buf.lock();
        let n = core::cmp::min(max, b.len());
        b.drain(..n).collect()
    }
}

/// `:get` output: the stream's pending bytes (a `Blob`).
static STD_METHODS: [MethodDesc; 1] = [MethodDesc {
    name: "get",
    input: &schema::SCHEMA_UNIT,
    output: &schema::SCHEMA_BLOB,
}];

impl Object for StdStream {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Device
    }

    fn value_schema(&self) -> &'static Schema {
        &schema::SCHEMA_BLOB
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &STD_METHODS
    }

    fn read_value(&self, out: &mut Vec<u8>, max: usize) -> Result<(), UnispaceError> {
        out.extend_from_slice(&self.drain(max));
        Ok(())
    }

    // The std streams are append-only ring buffers: a pipe has no offsets, so
    // the syscall flags word (positioned-read / append / write-at) carries no
    // meaning here. Accept any flags and keep drain/append semantics — the
    // libc `fwrite` layer always marks the standard streams APPEND (bit 0),
    // and rejecting that would silently drop every printf-family write.
    fn read_value_flags(
        &self,
        out: &mut Vec<u8>,
        max: usize,
        _flags: u64,
    ) -> Result<(), UnispaceError> {
        self.read_value(out, max)
    }

    fn write_value(&self, v: Value) -> Result<(), UnispaceError> {
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        self.append(&bytes);
        Ok(())
    }

    fn write_value_flags(&self, v: Value, _flags: u64) -> Result<(), UnispaceError> {
        self.write_value(v)
    }

    fn invoke(&self, method: usize, _v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                #[cfg(target_arch = "x86_64")]
                {
                    // Blocking get needs a running task to park; in pure kernel
                    // context (no current task) it would busy-spin, so refuse.
                    let pc = crate::smp::current_per_cpu();
                    if pc.current_task.load(core::sync::atomic::Ordering::Relaxed).is_null() {
                        return Err(UnispaceError::Unsupported);
                    }
                    // Blocking get: park until the stream yields bytes.
                    loop {
                        if self.buf.lock().is_empty() {
                            crate::task::sleep_until(
                                crate::services::universal_timer::now_ns()
                                    .saturating_add(2_000_000),
                            );
                            continue;
                        }
                        let data = self.drain(STREAM_CAP);
                        schema::encode_value(&Value::Bytes(data), &schema::SCHEMA_BLOB, out)?;
                        return Ok(());
                    }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    let _ = out;
                    Err(UnispaceError::Unsupported)
                }
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}

/// `/proc/<pid>/std`: the directory exposing the task's three streams. Each
/// `resolve` clones the shared `Arc` handle, so reads/writes target the same
/// buffers the `ProcDir` owns for the task's whole lifetime.
struct StdDir {
    input: StdStream,
    output: StdStream,
    err: StdStream,
}

impl StdDir {
    fn of(p: &ProcDir) -> Self {
        StdDir {
            input: p.stdin.clone(),
            output: p.stdout.clone(),
            err: p.stderr.clone(),
        }
    }
}

impl Object for StdDir {
    fn kind(&self) -> ObjectKind {
        ObjectKind::Dir
    }

    fn value_schema(&self) -> &'static Schema {
        &super::super::DIR_SCHEMA
    }

    fn methods(&self) -> &'static [MethodDesc] {
        &[]
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Object>> {
        if name == "in" {
            return Some(Arc::new(self.input.clone()) as Arc<dyn Object>);
        }
        if name == "out" {
            return Some(Arc::new(self.output.clone()) as Arc<dyn Object>);
        }
        if name == "err" {
            return Some(Arc::new(self.err.clone()) as Arc<dyn Object>);
        }
        None
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        out.push(ListingEntry {
            name: String::from("in"),
            kind: ObjectKind::Device,
        });
        out.push(ListingEntry {
            name: String::from("out"),
            kind: ObjectKind::Device,
        });
        out.push(ListingEntry {
            name: String::from("err"),
            kind: ObjectKind::Device,
        });
        Ok(())
    }

    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut entries = Vec::new();
        self.list(&mut entries)?;
        super::super::encode_listing(entries, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }

    fn invoke(&self, _method: usize, _v: Value, _out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        Err(UnispaceError::MethodNotFound)
    }
}

// ── :spawn implementation ────────────────────────────────────────────

/// Load `path` as an ELF, build its address space, and spawn it as a task.
/// Mirrors the boot path in `task::load::load_init_from_esp`.  Records the
/// spawner as the child's parent (via `current_pid`) and passes `args` through
/// for the child to read at `/proc/self/args`. `caps` is the explicit subset,
/// validated against the parent.
fn spawn_proc(path: &str, args: &str, caps: Vec<crate::caps::Cap>, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    crate::drivers::serial::SerialPort::puts("[proc] spawn_caps path=");
    crate::drivers::serial::SerialPort::puts(path);
    crate::drivers::serial::SerialPort::puts(" args=");
    crate::drivers::serial::SerialPort::puts(args);
    crate::drivers::serial::SerialPort::puts(" caps=");
    crate::drivers::serial::SerialPort::put_u64(caps.len() as u64);
    crate::drivers::serial::SerialPort::puts("\n");
    //  : staged timing and pre-allocate ELF buffer to avoid
    // repeated heap reallocations while holding VFS locks (observed hang at
    // spawn_caps with HEAP spin and IF=0). Try to stat file first to size.
    let t0 = crate::services::universal_timer::now_ns();
    // Best-effort stat to pre-size Vec and avoid O(n^2) realloc copy during read.
    // Costs an extra `write(:stat)` + decode roundtrip (2× IrqMutex traversals
    // per spawn) but stays best-effort and reserves outside the VFS lock.
    let mut stat_out = Vec::new();
    let file_len: Option<usize> = {
        let stat_path = alloc::format!("{}:stat", path);
        if super::super::write(&stat_path, &[], &mut stat_out).is_ok() {
            if let Ok(v) = crate::unispace::schema::decode_value(&stat_out, &crate::unispace::provider::vfs::STAT_OUTPUT) {
                if let crate::unispace::schema::Value::Struct(fields) = v {
                    if let Some(crate::unispace::schema::Value::U64(sz)) = fields.get(1) {
                        // Gate: try_reserve_exact is below outside the lock; reject absurd sizes here.
                        let sz_usize = *sz as usize;
                        if sz_usize > 64 * 1024 * 1024 {
                            None
                        } else {
                            Some(sz_usize)
                        }
                    } else { None }
                } else { None }
            } else { None }
        } else { None }
    };
    let mut elf = Vec::new();
    if let Some(sz) = file_len {
        // Reserve once outside any VFS lock; use try_reserve to avoid panic on huge (already bounded above).
        let _ = elf.try_reserve_exact(sz);
        crate::drivers::serial::SerialPort::puts("[proc] stat size=");
        crate::drivers::serial::SerialPort::put_u64(sz as u64);
        crate::drivers::serial::SerialPort::puts("\n");
    }
    crate::drivers::serial::SerialPort::puts("[proc] read ELF start\n");
    let _t1 = crate::services::universal_timer::now_ns();
    if let Err(e) = super::super::read(path, &mut elf, file_len.unwrap_or(usize::MAX)) {
        crate::drivers::serial::SerialPort::puts("[proc] spawn read ELF failed\n");
        return Err(e);
    }
    crate::drivers::serial::SerialPort::puts("[proc] read ELF done bytes=");
    crate::drivers::serial::SerialPort::put_u64(elf.len() as u64);
    crate::drivers::serial::SerialPort::puts(" dt_ms=");
    crate::drivers::serial::SerialPort::put_u64((crate::services::universal_timer::now_ns() - t0)/1_000_000);
    crate::drivers::serial::SerialPort::puts("\n");

    let alloc = crate::mm::heap::get_phys_allocator_mut();
    let t2 = crate::services::universal_timer::now_ns();
    let (root, entry, user_stack_top, vm) =
        crate::task::load::create_process(&elf, alloc).map_err(|e| {
            crate::drivers::serial::SerialPort::puts("[proc] create_process failed: ");
            crate::drivers::serial::SerialPort::puts(e);
            crate::drivers::serial::SerialPort::puts("\n");
            UnispaceError::DecodeError
        })?;
    crate::drivers::serial::SerialPort::puts("[proc] create_process done dt_ms=");
    crate::drivers::serial::SerialPort::put_u64((crate::services::universal_timer::now_ns() - t2)/1_000_000);
    crate::drivers::serial::SerialPort::puts("\n");

    let (kernel_stack_top, slot) =
        crate::task::alloc_kernel_stack(alloc).ok_or(UnispaceError::Unsupported)?;

    // Capability handling: explicit subset, validated
    let child_caps: Vec<crate::caps::Cap> = {
        let provided = caps;
        for c in &provided {
            if let Err(e) = crate::caps::validate_cap(c) {
                crate::drivers::serial::SerialPort::puts("[proc] validate_cap failed for ");
                crate::drivers::serial::SerialPort::puts(&c.path);
                crate::drivers::serial::SerialPort::puts(":");
                if let Some(m) = &c.method { crate::drivers::serial::SerialPort::puts(m); }
                crate::drivers::serial::SerialPort::puts("\n");
                return Err(e);
            }
            if c.perm as u8 == 2 {
                crate::drivers::serial::SerialPort::puts("[proc] invalid perm 2\n");
                return Err(UnispaceError::InvalidArgument);
            }
        }
        if provided.len() > crate::caps::MAX_CAPS_PER_TASK {
            crate::drivers::serial::SerialPort::puts("[proc] caps too many\n");
            return Err(UnispaceError::OutOfMemory);
        }
        // Subset check vs parent (kernel bypass when current_caps is None)
        if let Some(pc) = crate::caps::current_caps() {
            if !crate::caps::is_subset(&pc, &provided) {
                crate::drivers::serial::SerialPort::puts("[proc] subset check failed parent=");
                crate::drivers::serial::SerialPort::put_u64(pc.len() as u64);
                crate::drivers::serial::SerialPort::puts(" child=");
                crate::drivers::serial::SerialPort::put_u64(provided.len() as u64);
                crate::drivers::serial::SerialPort::puts("\n");
                for c in &provided {
                    if !crate::caps::has_perm(&pc, &c.path, c.method.as_deref(), c.perm) {
                        crate::drivers::serial::SerialPort::puts("  missing: ");
                        crate::drivers::serial::SerialPort::puts(&c.path);
                        if let Some(m)=&c.method { crate::drivers::serial::SerialPort::puts(":"); crate::drivers::serial::SerialPort::puts(m); }
                        crate::drivers::serial::SerialPort::puts(" perm=");
                        crate::drivers::serial::SerialPort::put_u64(c.perm as u64);
                        crate::drivers::serial::SerialPort::puts("\n");
                    }
                }
                return Err(UnispaceError::InvalidArgument);
            }
        } else {
            crate::drivers::serial::SerialPort::puts("[proc] no parent caps (kernel bypass)\n");
        }
        provided
    };

    // 5-word iretq frame at the top of the kernel stack (RIP, CS, RFLAGS,
    // RSP, SS) — `user_iret` pops exactly this.
    let frame_base = kernel_stack_top - 40;
    unsafe {
        *(frame_base as *mut u64) = entry; // RIP
        *(frame_base as *mut u64).add(1) = 0x2B; // user CS
        *(frame_base as *mut u64).add(2) = 0x202; // RFLAGS: IF set
        *(frame_base as *mut u64).add(3) = user_stack_top;
        *(frame_base as *mut u64).add(4) = 0x23; // user SS
    }

    let mut task = crate::task::Task::new(
        kernel_stack_top,
        root,
        0,
        crate::task::TaskContext::new(frame_base, crate::task::user_iret_addr()),
    );
    task.kstack_slot = slot;
    task.vm = vm;
    task.args = String::from(args);
    // Randomize the child's supervisor caps window base before installing it.
    task.caps_slot_va = crate::mm::layout::pick_caps_va();
    task.parent_pid = current_pid().unwrap_or(0);
    // Install caps page for child before spawn (maps the randomized window in child's root)
    if !child_caps.is_empty() {
        if let Some(phys) = crate::task::install_caps(root, &child_caps, task.caps_slot_va, alloc) {
            task.caps_arc = Some(alloc::sync::Arc::new(child_caps));
            task.caps_phys = phys;
        } else {
            // Rollback: free kernel stack and root
            crate::task::free_kernel_stack(slot, alloc);
            crate::mm::vmm::destroy_root(root, alloc);
            return Err(UnispaceError::OutOfMemory);
        }
    } else {
        // No caps: still need empty caps page? Not required; leave phys 0 -> child will be deny-all (empty).
        // But we still want child to be able to map caps page lazily if needed later.
    }
    let pid = crate::task::spawn(task);
    crate::drivers::serial::SerialPort::puts("[proc] spawned pid=");
    crate::drivers::serial::SerialPort::put_u64(pid);
    crate::drivers::serial::SerialPort::puts("\n");
    attach(pid);
    let v = Value::Struct(vec![Value::U64(pid)]);
    schema::encode_value(&v, &SPAWN_OUTPUT, out)
}

// ── Method input helpers (bounded; never panic on request data) ──────

/// Update the current task's PKRU rights for one key and apply immediately.
/// `rights`: 0 = RW, 1 = read-only (WD bit), 2 = no access (AD+WD bits).
fn set_task_pkru(key: u8, rights: u8) -> Result<(), UnispaceError> {
    let pc = crate::smp::current_per_cpu();
    let ptr = pc.current_task.load(core::sync::atomic::Ordering::Relaxed);
    if ptr.is_null() {
        return Err(UnispaceError::NotFound);
    }
    // SAFETY: current_task points at the leaked Task running this syscall.
    let t = unsafe { &mut *(ptr as *mut crate::task::Task) };
    let wd = 1u32 << (2 * key as u32);
    let ad = 1u32 << (2 * key as u32 + 1);
    t.pkru &= !(wd | ad);
    match rights {
        0 => {}
        1 => t.pkru |= wd,
        _ => t.pkru |= wd | ad,
    }
    crate::arch::x86_64::cpufeat::pku_apply(t.pkru);
    Ok(())
}

/// Run one of the `usermem` mutations against the *current* task's address
/// space, translating the raw errno (`-EINVAL`/`-EFAULT`/`-ENOMEM`) into a
/// `UnispaceError`. No current vm `→ NotFound`; a malformed request only ever
/// produces `Err`, never a panic.
fn mem_method<T>(
    f: impl FnOnce(usize, &mut crate::mm::phys_alloc::BitmapAllocator) -> Result<T, i64>,
) -> Result<T, UnispaceError> {
    let vm = crate::task::current_vm().ok_or(UnispaceError::NotFound)?;
    let alloc = crate::mm::heap::get_phys_allocator_mut();
    f(vm, alloc).map_err(|e| match e {
        -12 => UnispaceError::OutOfMemory,
        -14 => UnispaceError::BadAddress,
        _ => UnispaceError::InvalidArgument,
    })
}

/// Extract a `u64` field from a struct-typed method input.
fn arg_u64(v: &Value, idx: usize) -> Result<u64, UnispaceError> {
    match v {
        Value::Struct(fields) => match fields.get(idx) {
            Some(Value::U64(n)) => Ok(*n),
            _ => Err(UnispaceError::SchemaMismatch),
        },
        _ => Err(UnispaceError::SchemaMismatch),
    }
}

/// Extract a `str` field from a struct-typed method input.
fn arg_str(v: &Value, idx: usize) -> Result<&str, UnispaceError> {
    match v {
        Value::Struct(fields) => match fields.get(idx) {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(UnispaceError::SchemaMismatch),
        },
        _ => Err(UnispaceError::SchemaMismatch),
    }
}

/// Extract caps list from a struct-typed method input (index 2).
fn arg_caps(v: &Value, idx: usize) -> Result<Vec<crate::caps::Cap>, UnispaceError> {
    let caps_val = match v {
        Value::Struct(fields) => fields.get(idx).ok_or(UnispaceError::SchemaMismatch)?,
        _ => return Err(UnispaceError::SchemaMismatch),
    };
    let list = match caps_val {
        Value::List(items) => items,
        _ => return Err(UnispaceError::SchemaMismatch),
    };
    let mut out = Vec::new();
    for item in list {
        let fields = match item {
            Value::Struct(f) => f,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        if fields.len() != 3 {
            return Err(UnispaceError::SchemaMismatch);
        }
        let path = match &fields[0] {
            Value::Str(s) => s.clone(),
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        let method_raw = match &fields[1] {
            Value::Str(s) => s.clone(),
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        let method = if method_raw.is_empty() { None } else { Some(method_raw) };
        let perm_u32 = match &fields[2] {
            Value::U64(n) => *n as u32,
            _ => return Err(UnispaceError::SchemaMismatch),
        };
        let perm = match perm_u32 {
            1 => crate::caps::Perm::R,
            3 => crate::caps::Perm::RW,
            _ => return Err(UnispaceError::InvalidArgument),
        };
        out.push(crate::caps::Cap { path, method, perm });
    }
    Ok(out)
}
