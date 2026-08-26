//! SMP and scheduler introspection.
//!
//! - `/sys/smp/count` RO (already in /sys/cpus) — detailed per-CPU under `/kernel/smp`
//! - `/kernel/sched` family for queues, lineage, kstacks

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::super::schema::{self, Field, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    // smp detailed
    crate::unispace::connect("/kernel/smp/cpus", Arc::new(SmpCpusObject))?;
    crate::unispace::connect("/kernel/smp/states", Arc::new(SmpStatesObject))?;
    crate::unispace::connect("/kernel/smp/ap_ready", Arc::new(ApReadyObject))?;
    // sched
    crate::unispace::connect("/kernel/sched/snapshot", Arc::new(SchedSnapshotObject))?;
    crate::unispace::connect("/kernel/sched/queue", Arc::new(SchedQueueObject))?;
    crate::unispace::connect("/kernel/sched/lineage", Arc::new(LineageObject))?;
    crate::unispace::connect("/kernel/sched/kstacks", Arc::new(KstacksObject))?;
    // RO counts under /sys for pure introspection
    crate::unispace::connect("/sys/smp/count", Arc::new(SysSmpCountObject))?;
    crate::unispace::connect("/sys/sched/counts", Arc::new(SysSchedCountsObject))?;
    Ok(())
}

// ── /kernel/smp/cpus ──
static CPU_ENTRY: Schema = Schema::Struct(&[
    Field { name: "cpu_id", ty: &schema::SCHEMA_U32 },
    Field { name: "apic_id", ty: &schema::SCHEMA_U32 },
    Field { name: "is_bsp", ty: &schema::SCHEMA_BOOL },
    Field { name: "state", ty: &schema::SCHEMA_U32 },
    Field { name: "stack_top", ty: &schema::SCHEMA_U64 },
    Field { name: "has_task", ty: &schema::SCHEMA_BOOL },
    Field { name: "preempt", ty: &schema::SCHEMA_U32 },
    Field { name: "ticks", ty: &schema::SCHEMA_U64 },
]);
static CPU_LIST: Schema = Schema::List(&CPU_ENTRY);

struct SmpCpusObject;
impl Object for SmpCpusObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &CPU_LIST }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::smp::smp_snapshot();
        let mut items = Vec::with_capacity(snap.len());
        for (cpu_id, apic_id, is_bsp, state, stack_top, has_task, preempt, ticks) in snap {
            items.push(Value::Struct(vec![
                Value::U64(cpu_id as u64),
                Value::U64(apic_id as u64),
                Value::Bool(is_bsp),
                Value::U64(state as u64),
                Value::U64(stack_top),
                Value::Bool(has_task),
                Value::U64(preempt as u64),
                Value::U64(ticks),
            ]));
        }
        schema::encode_value(&Value::List(items), &CPU_LIST, out)
    }
}

// ── /kernel/smp/states ──
static STATES_SCHEMA: Schema = Schema::List(&schema::SCHEMA_U32);
struct SmpStatesObject;
impl Object for SmpStatesObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &STATES_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let arr = crate::smp::cpu_states_snapshot();
        let items = arr.iter().map(|&s| Value::U64(s as u64)).collect();
        schema::encode_value(&Value::List(items), &STATES_SCHEMA, out)
    }
}

// ── /kernel/smp/ap_ready ──
static AP_READY_SCHEMA: Schema = Schema::List(&schema::SCHEMA_BOOL);
struct ApReadyObject;
impl Object for ApReadyObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &AP_READY_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let arr = crate::smp::ap_ready_snapshot();
        let items = arr.iter().map(|&b| Value::Bool(b)).collect();
        schema::encode_value(&Value::List(items), &AP_READY_SCHEMA, out)
    }
}

// ── /kernel/sched/snapshot ──
static SCHED_SNAP: Schema = Schema::Struct(&[
    Field { name: "next_id", ty: &schema::SCHEMA_U64 },
    Field { name: "qlen", ty: &schema::SCHEMA_U64 },
    Field { name: "has_current", ty: &schema::SCHEMA_BOOL },
    Field { name: "sleeping", ty: &schema::SCHEMA_U64 },
    Field { name: "waiters", ty: &schema::SCHEMA_U64 },
    Field { name: "zombies", ty: &schema::SCHEMA_U64 },
    Field { name: "reclaim", ty: &schema::SCHEMA_U64 },
    Field { name: "kstack_used", ty: &schema::SCHEMA_U64 },
]);

struct SchedSnapshotObject;
impl Object for SchedSnapshotObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &SCHED_SNAP }
    #[cfg(target_arch = "x86_64")]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let (next_id, qlen, has_cur, sleeping, waiters, zombies, reclaim, kstack_used) = crate::task::sched_snapshot();
        let v = Value::Struct(vec![
            Value::U64(next_id),
            Value::U64(qlen as u64),
            Value::Bool(has_cur),
            Value::U64(sleeping as u64),
            Value::U64(waiters as u64),
            Value::U64(zombies as u64),
            Value::U64(reclaim as u64),
            Value::U64(kstack_used as u64),
        ]);
        schema::encode_value(&v, &SCHED_SNAP, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::Struct(vec![Value::U64(0), Value::U64(0), Value::Bool(false), Value::U64(0), Value::U64(0), Value::U64(0), Value::U64(0), Value::U64(0)]);
        schema::encode_value(&v, &SCHED_SNAP, out)
    }
}

// ── /kernel/sched/queue ──
static QUEUE_ENTRY: Schema = Schema::Struct(&[
    Field { name: "pid", ty: &schema::SCHEMA_U64 },
    Field { name: "state", ty: &schema::SCHEMA_U32 },
]);
static QUEUE_LIST: Schema = Schema::List(&QUEUE_ENTRY);

struct SchedQueueObject;
impl Object for SchedQueueObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &QUEUE_LIST }
    #[cfg(target_arch = "x86_64")]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::task::queue_snapshot();
        let items = snap.into_iter().map(|(pid, state)| Value::Struct(vec![Value::U64(pid), Value::U64(state as u64)])).collect();
        schema::encode_value(&Value::List(items), &QUEUE_LIST, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        schema::encode_value(&Value::List(Vec::new()), &QUEUE_LIST, out)
    }
}

// ── /kernel/sched/lineage ──
static LINEAGE_ENTRY: Schema = Schema::Struct(&[
    Field { name: "pid", ty: &schema::SCHEMA_U64 },
    Field { name: "ppid", ty: &schema::SCHEMA_U64 },
]);
static LINEAGE_LIST: Schema = Schema::List(&LINEAGE_ENTRY);

struct LineageObject;
impl Object for LineageObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &LINEAGE_LIST }
    #[cfg(target_arch = "x86_64")]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::task::lineage_snapshot();
        let items = snap.into_iter().map(|(pid, ppid)| Value::Struct(vec![Value::U64(pid), Value::U64(ppid)])).collect();
        schema::encode_value(&Value::List(items), &LINEAGE_LIST, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        schema::encode_value(&Value::List(Vec::new()), &LINEAGE_LIST, out)
    }
}

// ── /kernel/sched/kstacks ──
static KSTACK_ENTRY: Schema = Schema::Struct(&[
    Field { name: "slot", ty: &schema::SCHEMA_U64 },
    Field { name: "base", ty: &schema::SCHEMA_U64 },
]);
static KSTACK_LIST: Schema = Schema::List(&KSTACK_ENTRY);

struct KstacksObject;
impl Object for KstacksObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &KSTACK_LIST }
    #[cfg(target_arch = "x86_64")]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let snap = crate::task::kstack_snapshot();
        let items = snap.into_iter().map(|(slot, base)| Value::Struct(vec![Value::U64(slot as u64), Value::U64(base)])).collect();
        schema::encode_value(&Value::List(items), &KSTACK_LIST, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        schema::encode_value(&Value::List(Vec::new()), &KSTACK_LIST, out)
    }
}

// ── /sys/smp/count ──
struct SysSmpCountObject;
impl Object for SysSmpCountObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U32 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        schema::encode_value(&Value::U64(crate::smp::cpu_count() as u64), &schema::SCHEMA_U32, out)
    }
}

// ── /sys/sched/counts (RO counts) ──
static COUNTS_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "qlen", ty: &schema::SCHEMA_U64 },
    Field { name: "sleeping", ty: &schema::SCHEMA_U64 },
    Field { name: "waiters", ty: &schema::SCHEMA_U64 },
    Field { name: "zombies", ty: &schema::SCHEMA_U64 },
]);
struct SysSchedCountsObject;
impl Object for SysSchedCountsObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &COUNTS_SCHEMA }
    #[cfg(target_arch = "x86_64")]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let (_, qlen, _, sleeping, waiters, zombies, _, _) = crate::task::sched_snapshot();
        let v = Value::Struct(vec![Value::U64(qlen as u64), Value::U64(sleeping as u64), Value::U64(waiters as u64), Value::U64(zombies as u64)]);
        schema::encode_value(&v, &COUNTS_SCHEMA, out)
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::Struct(vec![Value::U64(0), Value::U64(0), Value::U64(0), Value::U64(0)]);
        schema::encode_value(&v, &COUNTS_SCHEMA, out)
    }
}
