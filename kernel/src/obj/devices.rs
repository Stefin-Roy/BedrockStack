//! S6 — Device family nodes: census nodes for PCI, input, and audio (§7.11).
//!
//! The PCI controller forest (§7.11.4) is a *real* family (§2.4):
//! `PciForestNode` is the family root and `materialize_pci_tree` realizes one
//! `PciDeviceNode` child per discovered device (parent edge = the forest root)
//! so P5 cascade revocation can sever the whole complex in one operation
//! (§3.7.2) and the §8.6 latency budget can be measured over a dozen-plus
//! devices. Input and audio remain census-only singletons. Every node exposes
//! a `count` hook that replies `[U64]`; they are seeded into the Boot domain
//! at bootstrap (§7.11.3–7.11.6) so a driver domain can probe device families
//! without ambient enumeration.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Once;

use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::CapRights;
use super::store::object_store;
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
under this root (0 until materialize_pci_tree runs).";

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

// ── PciDeviceNode — a materialized PCI child (§7.11.4) ─────────────────

/// Dynamic child-id base for PCI devices: `0x11_3000 + offset`, where the
/// offset packs the device's PCI address (segment:bus:device:function) so the
/// id is deterministic for a given slot (§7.8 stable-identity convention).
pub const PCI_DEVICE_CHILD_ID_BASE: u64 = 0x11_3000;

/// Small surface schema for a `PciDeviceNode` — the discoverable attributes a
/// projection tool renders per child (§4.1, §7.13). Contracts are empty for
/// now; dispatch is the P1 stub.
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

/// A materialized PCI device child under the `pci:forest` root. Carries a
/// `Copy` of the discovered `PciDevice`; its `ObjId` is derived from the
/// device's PCI address so the same slot always names the same child (§2.1,
/// §7.8). No contracts yet — a leaf the P5 cascade test can sever at the
/// trunk (§3.7.2).
pub struct PciDeviceNode {
    dev: crate::pci::PciDevice,
}

impl PciDeviceNode {
    /// `0x11_3000` plus the device's PCI address (segment:bus:device:function)
    /// packed into the low 32 bits — deterministic and unique per slot.
    fn obj_id_of(dev: &crate::pci::PciDevice) -> ObjId {
        let loc = (u64::from(dev.segment) << 16)
            | (u64::from(dev.bus) << 8)
            | (u64::from(dev.device) << 3)
            | u64::from(dev.function);
        ObjId(PCI_DEVICE_CHILD_ID_BASE + loc)
    }
}

impl Obj for PciDeviceNode {
    fn obj_id(&self) -> ObjId {
        Self::obj_id_of(&self.dev)
    }

    fn kind(&self) -> &'static str {
        "pci:device"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&PCI_DEVICE_SURFACE)
    }

    fn contracts(&self) -> &'static [ContractId] {
        &[]
    }

    fn dispatch(
        &self,
        _caller: &super::table::CapabilityTable,
        _rights: &CapRights,
        _hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        Err(ObjError::NotSupported)
    }
}

/// Materialize one PCI device as a child node under the forest root
/// `root_id` (§3.5, §7.11.4). Registers it in the store with
/// `parent = Some(root_id)` so the P5 projection sees the family edge
/// (§2.1, §7.13); returns the node.
pub fn materialize_pci_child(root_id: ObjId, dev: &crate::pci::PciDevice) -> Arc<dyn Obj> {
    let node: Arc<dyn Obj> = Arc::new(PciDeviceNode { dev: *dev });
    object_store().register_with_id_weak(node.obj_id(), node.kind(), Some(root_id), Some(root_id), &node);
    node
}

/// Module-level cache of the materialized PCI children, so the census `count`
/// / `children` hooks keep answering and re-materialization is idempotent.
static PCI_CHILDREN: Once<Vec<Arc<dyn Obj>>> = Once::new();

/// Materialize one child per discovered PCI device under the forest root
/// (§3.7.2, §7.11.4). Called by the coordinator from the run sequence after
/// PCI enumeration; no-op on riscv64 (the PCI subsystem is x86_64-concrete).
/// Idempotent: the first call populates the `PCI_CHILDREN` cache; later calls
/// return immediately.
pub fn materialize_pci_tree() {
    #[cfg(target_arch = "x86_64")]
    {
        PCI_CHILDREN.call_once(|| {
            crate::pci::devices()
                .iter()
                .map(|dev| materialize_pci_child(PCI_FOREST_OBJ_ID, dev))
                .collect::<Vec<Arc<dyn Obj>>>()
        });
    }
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
