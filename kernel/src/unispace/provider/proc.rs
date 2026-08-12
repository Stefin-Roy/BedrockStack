//! `/proc` provider — the process pseudo-filesystem (x86_64 only).
//!
//! `/proc/<pid>` is a directory per live task: tasks are `attach`ed here at
//! spawn/mount into the scheduler and `detach`ed at reap, so a clock tick
//! needs no registry of its own. `/proc/self` aliases the caller (and is
//! absent in kernel context, where no task is running).
//!
//! Each `<pid>` dir exposes:
//!   - `status` — `read` yields `{pid, state}` from the scheduler snapshot;
//!   - `:exit`  — write `{code}` to terminate the current task (diverges);
//!   - `:yield` — write `()` to cooperatively reschedule;
//!   - `:kill`  — write `{pid}` to park another task for reaping;
//!   - `:spawn` — write `{path}` to fork a new process from an ELF on a
//!     mounted filesystem.
//!
//! `ProcDir` stores only its `pid` — never a `&'static mut Task`. Dead tasks
//! are reaped with their boxes, and `detach` runs there, so a stale entry can
//! never outlive its task.

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
    EnumVariant { name: "ready", value: 0 },
    EnumVariant { name: "running", value: 1 },
    EnumVariant { name: "zzz", value: 2 },
    EnumVariant { name: "dead", value: 3 },
];

/// `read(/proc/<pid>/status)`: `struct{ pid: u64, state: enum }`.
static STATUS: Schema = Schema::Struct(&[
    schema::Field { name: "pid", ty: &schema::SCHEMA_U64 },
    schema::Field { name: "state", ty: &Schema::Enum(&PROC_STATE_VARIANTS) },
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

/// `write(/proc/<pid>:spawn, { path })`.
static SPAWN_INPUT: Schema = Schema::Struct(&[schema::Field {
    name: "path",
    ty: &schema::SCHEMA_STR,
}]);

/// `spawn` output: the new task's pid.
static SPAWN_OUTPUT: Schema = Schema::Struct(&[schema::Field {
    name: "pid",
    ty: &schema::SCHEMA_U64,
}]);

static PROC_METHODS: [MethodDesc; 4] = [
    MethodDesc { name: "exit", input: &EXIT_INPUT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "yield", input: &schema::SCHEMA_UNIT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "kill", input: &KILL_INPUT, output: &schema::SCHEMA_UNIT },
    MethodDesc { name: "spawn", input: &SPAWN_INPUT, output: &SPAWN_OUTPUT },
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
    procs().lock().insert(pid_key(pid), Arc::new(ProcDir { pid }));
}

/// Deregister a task's directory (called from `reap_dead` before the task
/// box is dropped).
pub fn detach(pid: u64) {
    procs().lock().remove(&pid_key(pid));
}

/// The running task's pid, or `None` in kernel context (current_task null).
fn current_pid() -> Option<u64> {
    let pc = crate::smp::current_per_cpu();
    if pc.current_task.is_null() {
        return None;
    }
    let t = unsafe { &*(pc.current_task as *const crate::task::Task) };
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
            return procs().lock().get(&pid_key(cur)).cloned().map(|p| p as Arc<dyn Object>);
        }
        procs().lock().get(name).cloned().map(|p| p as Arc<dyn Object>)
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        let guard = procs().lock();
        let mut names: Vec<String> = guard.keys().cloned().collect();
        names.sort();
        for n in names {
            out.push(ListingEntry { name: n, kind: ObjectKind::Dir });
        }
        if current_pid().is_some() {
            out.push(ListingEntry { name: String::from("self"), kind: ObjectKind::Dir });
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
        None
    }

    fn list(&self, out: &mut Vec<ListingEntry>) -> Result<(), UnispaceError> {
        out.push(ListingEntry {
            name: String::from("status"),
            kind: ObjectKind::Service,
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
                crate::task::yield_now();
                Ok(())
            }
            2 => {
                let target = arg_u64(&v, 0)?;
                crate::task::kill(target).map_err(|_| UnispaceError::NotFound)
            }
            3 => {
                let path = arg_str(&v, 0)?;
                spawn_proc(path, out)
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
        let v = Value::Struct(vec![Value::U64(self.pid), Value::Enum(disc)]);
        schema::encode_value(&v, &STATUS, out)
    }

    fn write_value(&self, _v: Value) -> Result<(), UnispaceError> {
        Err(UnispaceError::Unsupported)
    }
}

// ── :spawn implementation ────────────────────────────────────────────

/// Load `path` as an ELF, build its address space, and spawn it as a task.
/// Mirrors the boot path in `task::load::load_init_from_esp`.
fn spawn_proc(path: &str, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
    let mut elf = Vec::new();
    super::super::read(path, &mut elf, usize::MAX)?;

    let alloc = crate::mm::heap::get_phys_allocator_mut();
    let (root, entry, user_stack_top) = crate::task::load::create_process(&elf, alloc)
        .map_err(|_| UnispaceError::DecodeError)?;

    let (kernel_stack_top, slot) =
        crate::task::alloc_kernel_stack(alloc).ok_or(UnispaceError::Unsupported)?;

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
    let pid = crate::task::spawn(task);
    attach(pid);
    let v = Value::Struct(vec![Value::U64(pid)]);
    schema::encode_value(&v, &SPAWN_OUTPUT, out)
}

// ── Method input helpers (bounded; never panic on request data) ──────

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