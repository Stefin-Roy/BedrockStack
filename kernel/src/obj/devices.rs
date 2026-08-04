//! S6 — Device family nodes: census-only singletons for PCI, input, and audio.
//!
//! Each node is a thin `Obj` adapter over an existing kernel subsystem counter,
//! exposing a single `count` hook that replies `[U64]`.  They are seeded into
//! the Boot domain at bootstrap (§7.11.3–7.11.6) so a driver domain can probe
//! device families without ambient enumeration.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Once;

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::{CapRights, ContractRights, Rights};
use super::surface::{SurfaceDesc, TypeTag};
use super::{Args, Obj, ObjError, ObjId, Reply, Value};

// ── Audio readiness flag ────────────────────────────────────────────

static AUDIO_READY: AtomicBool = AtomicBool::new(false);

/// Called from `lib.rs` after `crate::audio::init()` to flip the flag.
pub fn init_audio() {
    AUDIO_READY.store(true, Ordering::Release);
}

// ── PciForestNode ───────────────────────────────────────────────────

const PCI_FOREST_OBJ_ID: ObjId = ObjId(0x10_0010);

pub struct PciForestNode;

pub const PCI_FOREST_CONTRACT: ContractId =
    ContractId::of("pci:forest", &PCI_FOREST_SURFACE, &PCI_FOREST_HOOKS);
pub const PCI_FOREST_COUNT: HookId = HookId::of("count");

pub const PCI_FOREST_DOC: &str = "if you count(), you get back [U64] = the \
number of PCI devices discovered at boot.";

const PCI_FOREST_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "pci:forest",
    attrs: &[],
    events: &[],
};

const PCI_FOREST_HOOKS: &[HookSignature] = &[HookSignature {
    name: "count",
    params: &[],
    reply: ReplyTag::Data(&[TypeTag::U64]),
}];

static PCI_FOREST_CONTRACTS: &[ContractId] = &[PCI_FOREST_CONTRACT];

impl Obj for PciForestNode {
    fn obj_id(&self) -> ObjId {
        PCI_FOREST_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "pci:forest"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        PCI_FOREST_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == PCI_FOREST_COUNT {
            let n = crate::pci::devices().len() as u64;
            return Ok(Reply::Data(vec![Value::U64(n)]));
        }
        Err(ObjError::NotSupported)
    }
}

pub fn pci_forest_node() -> Arc<dyn Obj> {
    static NODE: Once<Arc<dyn Obj>> = Once::new();
    NODE.call_once(|| Arc::new(PciForestNode)).clone()
}

// ── InputFamilyNode ─────────────────────────────────────────────────

const INPUT_FAMILY_OBJ_ID: ObjId = ObjId(0x10_0011);

pub struct InputFamilyNode;

pub const INPUT_FAMILY_CONTRACT: ContractId =
    ContractId::of("input:family", &INPUT_FAMILY_SURFACE, &INPUT_FAMILY_HOOKS);
pub const INPUT_FAMILY_COUNT: HookId = HookId::of("count");

pub const INPUT_FAMILY_DOC: &str = "if you count(), you get back [U64] = the \
number of input devices registered with UInputL.";

const INPUT_FAMILY_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "input:family",
    attrs: &[],
    events: &[],
};

const INPUT_FAMILY_HOOKS: &[HookSignature] = &[HookSignature {
    name: "count",
    params: &[],
    reply: ReplyTag::Data(&[TypeTag::U64]),
}];

static INPUT_FAMILY_CONTRACTS: &[ContractId] = &[INPUT_FAMILY_CONTRACT];

impl Obj for InputFamilyNode {
    fn obj_id(&self) -> ObjId {
        INPUT_FAMILY_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "input:family"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        INPUT_FAMILY_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == INPUT_FAMILY_COUNT {
            let n = crate::input::device_count() as u64;
            return Ok(Reply::Data(vec![Value::U64(n)]));
        }
        Err(ObjError::NotSupported)
    }
}

pub fn input_family_node() -> Arc<dyn Obj> {
    static NODE: Once<Arc<dyn Obj>> = Once::new();
    NODE.call_once(|| Arc::new(InputFamilyNode)).clone()
}

// ── AudioFamilyNode ─────────────────────────────────────────────────

const AUDIO_FAMILY_OBJ_ID: ObjId = ObjId(0x10_0012);

pub struct AudioFamilyNode;

pub const AUDIO_FAMILY_CONTRACT: ContractId =
    ContractId::of("audio:family", &AUDIO_FAMILY_SURFACE, &AUDIO_FAMILY_HOOKS);
pub const AUDIO_FAMILY_COUNT: HookId = HookId::of("count");

pub const AUDIO_FAMILY_DOC: &str = "if you count(), you get back [U64] = 1 if \
an audio device was initialised, 0 otherwise.";

const AUDIO_FAMILY_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "audio:family",
    attrs: &[],
    events: &[],
};

const AUDIO_FAMILY_HOOKS: &[HookSignature] = &[HookSignature {
    name: "count",
    params: &[],
    reply: ReplyTag::Data(&[TypeTag::U64]),
}];

static AUDIO_FAMILY_CONTRACTS: &[ContractId] = &[AUDIO_FAMILY_CONTRACT];

impl Obj for AudioFamilyNode {
    fn obj_id(&self) -> ObjId {
        AUDIO_FAMILY_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "audio:family"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        AUDIO_FAMILY_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == AUDIO_FAMILY_COUNT {
            let n = if AUDIO_READY.load(Ordering::Acquire) { 1 } else { 0 };
            return Ok(Reply::Data(vec![Value::U64(n)]));
        }
        Err(ObjError::NotSupported)
    }
}

pub fn audio_family_node() -> Arc<dyn Obj> {
    static NODE: Once<Arc<dyn Obj>> = Once::new();
    NODE.call_once(|| Arc::new(AudioFamilyNode)).clone()
}

// ── Contract definitions (§7.8) ─────────────────────────────────────

static PCI_FOREST_CONTRACT_DEF: Contract = Contract {
    id: PCI_FOREST_CONTRACT,
    name: "pci:forest",
    surface: &PCI_FOREST_SURFACE,
    hooks: PCI_FOREST_HOOKS,
    doc: PCI_FOREST_DOC,
};

static INPUT_FAMILY_CONTRACT_DEF: Contract = Contract {
    id: INPUT_FAMILY_CONTRACT,
    name: "input:family",
    surface: &INPUT_FAMILY_SURFACE,
    hooks: INPUT_FAMILY_HOOKS,
    doc: INPUT_FAMILY_DOC,
};

static AUDIO_FAMILY_CONTRACT_DEF: Contract = Contract {
    id: AUDIO_FAMILY_CONTRACT,
    name: "audio:family",
    surface: &AUDIO_FAMILY_SURFACE,
    hooks: AUDIO_FAMILY_HOOKS,
    doc: AUDIO_FAMILY_DOC,
};

pub fn pci_forest_contract_def() -> &'static Contract {
    &PCI_FOREST_CONTRACT_DEF
}

pub fn input_family_contract_def() -> &'static Contract {
    &INPUT_FAMILY_CONTRACT_DEF
}

pub fn audio_family_contract_def() -> &'static Contract {
    &AUDIO_FAMILY_CONTRACT_DEF
}
