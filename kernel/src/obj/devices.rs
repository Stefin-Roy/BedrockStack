//! S6 — Device family nodes: census nodes for PCI, input, and audio (§7.11).
//!
//! `PciForestNode` is the PCI controller family root (§7.11.4). Input and
//! audio remain census-only singletons. Every node exposes a `count` hook that
//! replies `[U64]`; they are seeded into the Boot domain at bootstrap
//! (§7.11.3–7.11.6) so a driver domain can probe device families without
//! ambient enumeration.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Once;

use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::CapRights;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
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
pub const PCI_FOREST_CHILDREN: HookId = HookId::of("children");

pub const PCI_FOREST_DOC: &str = "the PCI controller forest family root \
(§7.11.4): count() replies [U64] = the number of PCI devices discovered at \
boot; children() replies [U64] = the number of device children materialized \
under this root.";

const PCI_FOREST_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "pci:forest",
    attrs: &[],
    events: &[],
};

const PCI_FOREST_HOOKS: &[HookSignature] = &[
    HookSignature { name: "count", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "children", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
];

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
        if hook == PCI_FOREST_CHILDREN {
            let n = PCI_CHILDREN.get().map_or(0, |v| v.len()) as u64;
            return Ok(Reply::Data(vec![Value::U64(n)]));
        }
        Err(ObjError::NotSupported)
    }
}

pub fn pci_forest_node() -> Arc<dyn Obj> {
    static NODE: Once<Arc<dyn Obj>> = Once::new();
    NODE.call_once(|| Arc::new(PciForestNode)).clone()
}

pub const PCI_DEVICE_CONTRACT: ContractId =
    ContractId::of("pci:device", &PCI_DEVICE_SURFACE, &PCI_DEVICE_HOOKS);

pub const PCI_DEVICE_DOC: &str = "the pci:device contract (§7.11.4): config-space \
read8/read16/read32/offset() and write8/write16/write32/offset,value() reach the \
ECAM config space of this device; vendor_id(), device_id(), class(), bar(index) \
and irq_line() read the discovered identity without touching config space. \
READ-gated hooks read; WRITE-gated hooks write (per-hook contract rights, §3.3).";

/// Surface schema for the `pci:device` contract — the discoverable attributes a
/// projection tool renders per device (§4.1, §7.13), reachable through the
/// contract as live identity reads.
const PCI_DEVICE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "pci:device",
    attrs: &[
        SurfaceAttr { name: "bus", ty: TypeTag::U64 },
        SurfaceAttr { name: "device", ty: TypeTag::U64 },
        SurfaceAttr { name: "function", ty: TypeTag::U64 },
        SurfaceAttr { name: "vendor_id", ty: TypeTag::U64 },
        SurfaceAttr { name: "device_id", ty: TypeTag::U64 },
        SurfaceAttr { name: "class", ty: TypeTag::U64 },
    ],
    events: &[],
};

const PCI_DEVICE_HOOKS: &[HookSignature] = &[
    HookSignature { name: "read8", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "read16", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "read32", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "write8", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "write16", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "write32", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "vendor_id", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "device_id", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "class", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "bar", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "irq_line", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
];

/// Module-level cache of the materialized PCI children, so the census
/// `children` hook keeps answering.
static PCI_CHILDREN: Once<Vec<Arc<dyn Obj>>> = Once::new();

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

static PCI_DEVICE_CONTRACT_DEF: Contract = Contract {
    id: PCI_DEVICE_CONTRACT,
    name: "pci:device",
    surface: &PCI_DEVICE_SURFACE,
    hooks: PCI_DEVICE_HOOKS,
    doc: PCI_DEVICE_DOC,
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

pub fn pci_device_contract_def() -> &'static Contract {
    &PCI_DEVICE_CONTRACT_DEF
}

pub fn input_family_contract_def() -> &'static Contract {
    &INPUT_FAMILY_CONTRACT_DEF
}

pub fn audio_family_contract_def() -> &'static Contract {
    &AUDIO_FAMILY_CONTRACT_DEF
}
