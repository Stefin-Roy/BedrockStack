//! The `io:stream` contract — a process's standard streams as first-class
//! graph objects (x86_64).
//!
//! Every task is endowed with three stream nodes — stdin/stdout/stderr — at
//! the fixed ABI slots 0/1/2. A stream owns an append-only byte history
//! (bounded, drop-oldest), so its surface reports both the full accumulated
//! `content` and the number of unconsumed `buffered` bytes. A `Serial`-kind
//! stdout/stderr echoes its writes to the kernel's COM1 console (preserving
//! the pre-stream `sys_write` behaviour); an `Input`-kind stdin can be wired
//! to UInputL via `connect_input`, after which a drained `read` pumps key
//! events through the keymap into the history.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::drivers::serial::SerialPort;
use crate::obj::contract::{Contract, ContractId, HookSignature, ReplyTag};
use crate::obj::hook::HookId;
use crate::obj::rights::{CapRights, ContractRights};
use crate::obj::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use crate::obj::table::CapabilityTable;
use crate::obj::{Args, Obj, ObjError, ObjId, Reply, Value};
use crate::services::irqsafe::IrqLock;

/// The `io:stream` contract.
pub const STREAM_CONTRACT: ContractId = ContractId::of("io:stream", &STREAM_SURFACE, &STREAM_HOOKS);
pub const STREAM_READ: HookId = HookId::of("read");
pub const STREAM_WRITE: HookId = HookId::of("write");
pub const STREAM_SIZE: HookId = HookId::of("size");
pub const STREAM_CONNECT_INPUT: HookId = HookId::of("connect_input");

pub const STREAM_DOC: &str = "if you write(buf), bytes are appended to the stream's \
history (a Serial-kind stdout/stderr also echoes to the console); read(n) serves \
up to n unconsumed bytes; size() reports how many bytes remain unconsumed; \
connect_input() wires an Input-kind stdin stream to the keyboard.";

const STREAM_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "io:stream",
    attrs: &[
        SurfaceAttr { name: "role", ty: TypeTag::Str },
        SurfaceAttr { name: "kind", ty: TypeTag::Str },
        SurfaceAttr { name: "buffered", ty: TypeTag::U64 },
        SurfaceAttr { name: "content", ty: TypeTag::Buf },
    ],
    events: &[],
};

const STREAM_HOOKS: &[HookSignature] = &[
    HookSignature { name: "read", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::Buf]) },
    HookSignature { name: "write", params: &[TypeTag::Buf], reply: ReplyTag::None },
    HookSignature { name: "size", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "connect_input", params: &[], reply: ReplyTag::None },
];

static STREAM_CONTRACTS: &[ContractId] = &[STREAM_CONTRACT];

static STREAM_CONTRACT_DEF: Contract = Contract {
    id: STREAM_CONTRACT,
    name: "io:stream",
    surface: &STREAM_SURFACE,
    hooks: STREAM_HOOKS,
    doc: STREAM_DOC,
};

/// The canonical definition of the io:stream contract.
pub fn stream_contract_def() -> &'static Contract {
    &STREAM_CONTRACT_DEF
}

/// Register the `io:stream` definition in the contract registry through the
/// boot domain's registry capability (mirrors `proc:task`; the registry's
/// `register` hook resolves names via `adapters::contract_def`).
pub fn register_stream_contract() {
    use crate::obj::bootstrap::{boot_domain, boot_endowment};
    use crate::obj::registry::{REGISTRY_CONTRACT, REGISTRY_REGISTER};
    let table = &boot_domain().table;
    let registry = boot_endowment().registry;
    let args = Args { vals: vec![Value::Str(STREAM_CONTRACT_DEF.name)] };
    let _ = crate::obj::invoke(table, registry, REGISTRY_CONTRACT, REGISTRY_REGISTER, &args);
}

/// Which standard stream a node implements.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamRole {
    Stdin,
    Stdout,
    Stderr,
}

impl StreamRole {
    pub const fn label(&self) -> &'static str {
        match self {
            StreamRole::Stdin => "stdin",
            StreamRole::Stdout => "stdout",
            StreamRole::Stderr => "stderr",
        }
    }
}

/// What a stream is wired to. A `Serial` stream echoes writes to the console;
/// an `Input` stream can be connected to UInputL. `Pipe`/`Null` are reserved
/// for future spawn-pipe wiring and sinks that swallow output.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Serial,
    Input,
    Pipe,
    Null,
}

impl StreamKind {
    pub const fn label(&self) -> &'static str {
        match self {
            StreamKind::Serial => "serial",
            StreamKind::Input => "input",
            StreamKind::Pipe => "pipe",
            StreamKind::Null => "null",
        }
    }
}

/// Cap on the append-only history: 64 KiB, oldest bytes dropped first.
const STREAM_BUF_CAP: usize = 64 * 1024;

/// Base of the dynamic per-task stream node id space (above the per-task
/// addrspace band `0x11_4000`, so node kinds never collide).
const STREAM_ID_BASE: u64 = 0x11_5000;

/// Next dynamic per-task stream node id.
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(STREAM_ID_BASE);

fn next_stream_id() -> ObjId {
    ObjId(NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed))
}

/// A standard stream: an append-only byte history plus a read cursor (stdin
/// only). `DropDeath` (lifetime = reachability): it is Arc'd by both the task's
/// TCB and the table caps, so it is freed when the task is reaped.
///
/// The buffer lock is a plain ordering-exempt `IrqLock` (order 0): streams are
/// only ever touched from the syscall/surface path, never from an ISR, so it
/// follows the `Task::parked` discipline.
pub struct StreamNode {
    id: ObjId,
    role: StreamRole,
    kind: StreamKind,
    /// Append-only byte history. Bounded; the oldest bytes are dropped first.
    buf: IrqLock<Vec<u8>>,
    /// Consumed prefix (stdin only). stdout/stderr never advance it.
    read_cursor: IrqLock<usize>,
    /// Whether UInputL is wired into this stream's `read` (stdin only).
    input_connected: AtomicBool,
}

impl StreamNode {
    /// Build a fresh stream node with a dynamic id.
    pub fn new(role: StreamRole, kind: StreamKind) -> Arc<Self> {
        Arc::new(StreamNode {
            id: next_stream_id(),
            role,
            kind,
            buf: IrqLock::new(Vec::new()),
            read_cursor: IrqLock::new(0),
            input_connected: AtomicBool::new(false),
        })
    }

    pub fn role_label(&self) -> &'static str {
        self.role.label()
    }

    pub fn kind_label(&self) -> &'static str {
        self.kind.label()
    }

    /// Append `bytes` to the history, bounded to [`STREAM_BUF_CAP`] with the
    /// oldest bytes dropped first.
    fn append(&self, bytes: &[u8]) {
        let mut buf = self.buf.lock();
        if bytes.len() >= STREAM_BUF_CAP {
            buf.clear();
            buf.extend_from_slice(&bytes[bytes.len() - STREAM_BUF_CAP..]);
            return;
        }
        let over = buf
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(STREAM_BUF_CAP);
        if over > 0 {
            buf.drain(..over);
        }
        buf.extend_from_slice(bytes);
    }

    /// Append to the history; a `Serial`-kind stream also echoes the bytes to
    /// the COM1 console (preserving the pre-stream `sys_write` output).
    pub fn write(&self, bytes: &[u8]) {
        self.append(bytes);
        if self.kind == StreamKind::Serial {
            for &b in bytes {
                SerialPort::putc(b);
            }
        }
    }

    /// Serve up to `max` unconsumed bytes, advancing the read cursor. On a
    /// `connect_input`-wired stdin whose history is drained, key events are
    /// pumped through the keymap into the history first.
    pub fn read(&self, max: usize) -> Vec<u8> {
        if self.role == StreamRole::Stdin && self.input_connected.load(Ordering::Relaxed) {
            let mut buf = self.buf.lock();
            let drained = buf.len() == *self.read_cursor.lock();
            if drained {
                let mut km = crate::input::keymap();
                while let Some(ev) = crate::input::read_event() {
                    if let Some(c) = km.feed(&ev) {
                        let mut tmp = [0u8; 4];
                        buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                }
            }
            drop(buf);
        }
        if max == 0 {
            return Vec::new();
        }
        let mut served = Vec::new();
        {
            let buf = self.buf.lock();
            let mut cur = self.read_cursor.lock();
            let avail = buf.len().saturating_sub(*cur);
            let n = max.min(avail);
            served.extend_from_slice(&buf[*cur..*cur + n]);
            *cur += n;
        }
        served
    }

    /// The full accumulated history (never consumes).
    pub fn content(&self) -> Vec<u8> {
        self.buf.lock().clone()
    }

    /// Bytes not yet consumed by a `read` (the cursor never advances on
    /// stdout/stderr, so this equals the history length there).
    pub fn buffered(&self) -> usize {
        let buf = self.buf.lock();
        let cur = self.read_cursor.lock();
        buf.len().saturating_sub(*cur)
    }

    /// Wire UInputL into this stream's `read` (stdin only). Returns `false`
    /// for any other role.
    pub fn connect_input(&self) -> bool {
        if self.role == StreamRole::Stdin {
            self.input_connected.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

impl Obj for StreamNode {
    fn obj_id(&self) -> ObjId {
        self.id
    }

    fn kind(&self) -> &'static str {
        "io:stream"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&STREAM_SURFACE)
    }

    fn surface_value<'a>(&self, name: &str) -> Option<Value<'a>> {
        match name {
            "role" => Some(Value::Str(self.role_label())),
            "kind" => Some(Value::Str(self.kind_label())),
            "buffered" => Some(Value::U64(self.buffered() as u64)),
            "content" => Some(Value::Buf(self.content())),
            _ => None,
        }
    }

    fn contracts(&self) -> &'static [ContractId] {
        STREAM_CONTRACTS
    }

    fn hook_contract_right(&self, _contract: ContractId, hook: HookId) -> ContractRights {
        match hook {
            STREAM_READ => ContractRights::READ,
            STREAM_WRITE => ContractRights::WRITE,
            _ => ContractRights::CALL,
        }
    }

    fn dispatch<'a>(
        &self,
        _caller: &CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args<'a>,
    ) -> Result<Reply<'a>, ObjError> {
        match hook {
            STREAM_READ => {
                let max = match args.vals.first() {
                    Some(Value::U64(v)) => (*v).min(4096) as usize,
                    _ => 4096,
                };
                Ok(Reply::Data(vec![Value::Buf(self.read(max))]))
            }
            STREAM_WRITE => {
                if let Some(Value::Buf(b)) = args.vals.first() {
                    self.write(b);
                }
                Ok(Reply::None)
            }
            STREAM_SIZE => Ok(Reply::Data(vec![Value::U64(self.buffered() as u64)])),
            STREAM_CONNECT_INPUT => {
                if self.connect_input() {
                    Ok(Reply::None)
                } else {
                    Err(ObjError::NotSupported)
                }
            }
            _ => Err(ObjError::NotSupported),
        }
    }
}
