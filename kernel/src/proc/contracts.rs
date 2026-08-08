//! The `proc:task` contract — the capability node that makes multitasking
//! reachable from ring 3 (x86_64).
//!
//! Every task is endowed with a root node at cap slot 8 exposing
//! `spawn`/`yield`/`kill`/`join`; `spawn` returns a child node cap (wrapping
//! the child's TCB) so the parent can `kill`/`join` it and read its live
//! surface (`state`, `id`, `domain_id`). The contract is registered in the
//! contract registry like any other provider, so `resolve(b"proc:task\0")`
//! answers from ring 3.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;

use crate::obj::cap_handle::{CapHandle, CapId, HandleState};
use crate::obj::contract::{Contract, ContractId, HookSignature, ReplyTag};
use crate::obj::hook::HookId;
use crate::obj::rights::{CapRights, ContractRights, Rights};
use crate::obj::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use crate::obj::table::CapabilityTable;
use crate::obj::{Args, Obj, ObjError, ObjId, Reply, Value};
use crate::proc::{current_task, kill_task, spawn_child, Task};

/// The `proc:task` contract.
pub const PROC_CONTRACT: ContractId = ContractId::of("proc:task", &PROC_SURFACE, &PROC_HOOKS);
pub const PROC_SPAWN: HookId = HookId::of("spawn");
pub const PROC_YIELD: HookId = HookId::of("yield");
pub const PROC_KILL: HookId = HookId::of("kill");
pub const PROC_JOIN: HookId = HookId::of("join");

pub const PROC_DOC: &str = "if you spawn(elf), a child task inheriting your \
capabilities is created and its node cap is returned; yield() parks your task \
until the scheduler cycles; kill(child) tears a child down; join(child) blocks \
until the child exits.";

const PROC_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "proc:task",
    attrs: &[
        SurfaceAttr { name: "domain_id", ty: TypeTag::U64 },
        SurfaceAttr { name: "task_id", ty: TypeTag::U64 },
    ],
    events: &[],
};

const PROC_HOOKS: &[HookSignature] = &[
    HookSignature { name: "spawn", params: &[TypeTag::Buf], reply: ReplyTag::Caps },
    HookSignature { name: "yield", params: &[], reply: ReplyTag::None },
    HookSignature { name: "kill", params: &[TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "join", params: &[TypeTag::U64], reply: ReplyTag::None },
];

static PROC_CONTRACTS: &[ContractId] = &[PROC_CONTRACT];

static PROC_CONTRACT_DEF: Contract = Contract {
    id: PROC_CONTRACT,
    name: "proc:task",
    surface: &PROC_SURFACE,
    hooks: PROC_HOOKS,
    doc: PROC_DOC,
};

/// The canonical definition of the proc:task contract.
pub fn proc_contract_def() -> &'static Contract {
    &PROC_CONTRACT_DEF
}

/// Register the `proc:task` definition in the contract registry through the
/// boot domain's registry capability (mirrors the fs/infra provider seeding).
pub fn register_proc_contract() {
    use crate::obj::bootstrap::{boot_domain, boot_endowment};
    use crate::obj::registry::{REGISTRY_CONTRACT, REGISTRY_REGISTER};
    let table = &boot_domain().table;
    let registry = boot_endowment().registry;
    let args = Args { vals: vec![Value::Str(PROC_CONTRACT_DEF.name)] };
    let _ = crate::obj::invoke(table, registry, REGISTRY_CONTRACT, REGISTRY_REGISTER, &args);
}

/// Stable identities for the proc:task nodes.
const PROC_OBJ_ID: ObjId = ObjId(0x10_000A);
const TASK_NODE_OBJ_ID: ObjId = ObjId(0x10_000B);

/// The root node endowed to every task at slot 8. A unit node: `yield` and
/// `spawn` operate on the *current* task (the cap holder); `kill`/`join`
/// resolve a child node cap in the caller's own table, so there is no ambient
/// authority — a task can only manage tasks it holds a cap to.
pub struct ProcRootNode;

impl Obj for ProcRootNode {
    fn obj_id(&self) -> ObjId {
        PROC_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "proc:task"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&PROC_SURFACE)
    }

    fn surface_value<'a>(&self, name: &str) -> Option<Value<'a>> {
        let task = current_task()?;
        match name {
            "domain_id" => Some(Value::U64(task.domain.id as u64)),
            "task_id" => Some(Value::U64(task.id as u64)),
            _ => None,
        }
    }

    fn contracts(&self) -> &'static [ContractId] {
        PROC_CONTRACTS
    }

    fn dispatch<'a>(
        &self,
        caller: &CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args<'a>,
    ) -> Result<Reply<'a>, ObjError> {
        match hook {
            PROC_SPAWN => {
                let buf = match args.vals.first() {
                    Some(Value::Buf(b)) => b.clone(),
                    _ => return Err(ObjError::Denied),
                };
                let task = spawn_child(&buf).map_err(|_| ObjError::OutOfMemory)?;
                let h = CapHandle {
                    id: CapId(0),
                    node: Arc::new(TaskNode { task }),
                    rights: CapRights::new(
                        Rights::INVOKE.or(Rights::QUERY),
                        ContractRights::READ.or(ContractRights::WRITE).or(ContractRights::CALL),
                    ),
                    state: HandleState::Live,
                };
                Ok(Reply::Caps(vec![h]))
            }
            PROC_YIELD => crate::proc::yield_current(),
            PROC_KILL => {
                let id = arg_u64(args, 0).ok_or(ObjError::Denied)?;
                let task = child_task(caller, id)?;
                kill_task(&task);
                Ok(Reply::None)
            }
            PROC_JOIN => {
                let id = arg_u64(args, 0).ok_or(ObjError::Denied)?;
                let task = child_task(caller, id)?;
                crate::proc::join_park(task)
            }
            _ => Err(ObjError::NotSupported),
        }
    }
}

/// A cap wrapping a child task's TCB, returned by `spawn`. It advertises
/// `proc:task` so the PERMIT path lets a parent `kill`/`join` through it, and
/// exposes the child's live surface; the actual hooks run on the root node
/// with the child cap id as the argument.
pub struct TaskNode {
    pub task: Arc<Task>,
}

impl TaskNode {
    pub fn task(&self) -> &Arc<Task> {
        &self.task
    }
}

impl Obj for TaskNode {
    fn obj_id(&self) -> ObjId {
        TASK_NODE_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "proc:task-node"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&TASK_NODE_SURFACE)
    }

    fn surface_value<'a>(&self, name: &str) -> Option<Value<'a>> {
        match name {
            "state" => Some(Value::U64(self.task.state() as u64)),
            "id" => Some(Value::U64(self.task.id as u64)),
            "domain_id" => Some(Value::U64(self.task.domain.id as u64)),
            _ => None,
        }
    }

    fn contracts(&self) -> &'static [ContractId] {
        PROC_CONTRACTS
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn dispatch<'a>(
        &self,
        _caller: &CapabilityTable,
        _rights: &CapRights,
        _hook: HookId,
        _args: &Args<'a>,
    ) -> Result<Reply<'a>, ObjError> {
        Err(ObjError::NotSupported)
    }
}

const TASK_NODE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "proc:task-node",
    attrs: &[
        SurfaceAttr { name: "state", ty: TypeTag::U64 },
        SurfaceAttr { name: "id", ty: TypeTag::U64 },
        SurfaceAttr { name: "domain_id", ty: TypeTag::U64 },
    ],
    events: &[],
};

/// The proc:task root node, for endowing a task (§7.8).
pub fn proc_root_node() -> Arc<dyn Obj> {
    Arc::new(ProcRootNode)
}

fn arg_u64(args: &Args<'_>, i: usize) -> Option<u64> {
    match args.vals.get(i) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

/// Resolve a child-node cap in the caller's table and recover the child TCB.
fn child_task(caller: &CapabilityTable, id: u64) -> Result<Arc<Task>, ObjError> {
    let node = caller
        .resolve(CapId(id), PROC_CONTRACT, PROC_KILL)
        .map_err(|_| ObjError::Denied)?;
    let tn = node
        .as_any()
        .and_then(|a| a.downcast_ref::<TaskNode>())
        .ok_or(ObjError::Denied)?;
    Ok(Arc::clone(&tn.task))
}
