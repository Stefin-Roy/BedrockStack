//! C4 — `Obj` adapters for the kernel service providers (§7.2.1, §7.11.4).
//!
//! The Boot domain holds a capability to each provider and reaches it through
//! the `invoke` entry (§7.9) — full invoke form, no downcast fast path. Each
//! adapter wraps an existing kernel service provider (implementing `Obj` by
//! delegating `dispatch` to the same provider-trait methods the object already
//! uses) so a hook call reaches the singleton's real internal state.
//!
//! All adapters are arch-neutral.

use alloc::sync::Arc;
use alloc::vec;

use crate::mm::vmm::PageFlags;
use crate::services::ecam_pci_config::EcamPciConfig;
use crate::services::pci_config::PciConfigSpace;
use crate::services::serial::SerialConsole;
use crate::services::serial::KernelSerial;

macro_rules! dma_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "dma_trace")]
        $($arg)*
    };
}
use crate::services::dma::KernelDma;

use super::cap_handle::RevocationPolicy;
use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::memregion;
use super::nodes;
use super::rights::CapRights;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::table::CapabilityTable;
use super::devices;
use super::fs;
use super::{invoke, Args, Obj, ObjError, ObjId, Reply, Value};

// ── Stable identities (singletons; deterministic, never from the store) ──

const DMA_OBJ_ID: ObjId = ObjId(0x10_0000);
const PCI_OBJ_ID: ObjId = ObjId(0x10_0001);
const SERIAL_OBJ_ID: ObjId = ObjId(0x10_0002);

// ── C5 accessors: concrete provider singletons as `Arc<dyn Obj>` ──────────
//
// The providers are process-lifetime `'static` singletons, so a capability may
// name one without ever being consumed. `NodeRef` forwards every `Obj` method
// to the concrete node (whose `Obj` impl lives above), keeping the upstream
// service files untouched.

struct NodeRef<T: ?Sized + 'static>(&'static T);

impl<T: Obj + ?Sized> Obj for NodeRef<T> {
    fn obj_id(&self) -> ObjId {
        self.0.obj_id()
    }

    fn kind(&self) -> &'static str {
        self.0.kind()
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        self.0.surface()
    }

    fn contracts(&self) -> &'static [ContractId] {
        self.0.contracts()
    }

    fn revocation(&self) -> RevocationPolicy {
        self.0.revocation()
    }

    fn dispatch(
        &self,
        caller: &CapabilityTable,
        rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        self.0.dispatch(caller, rights, hook, args)
    }
}

/// C5: the DMA provider node, for endowing the boot domain (§5.4).
pub fn dma_node() -> Arc<dyn Obj> {
    Arc::new(NodeRef(crate::services::dma::dma_allocator_static()))
}

/// C5: the PCI config-space provider node, for endowing the boot domain.
pub fn pci_cfg_node() -> Arc<dyn Obj> {
    Arc::new(NodeRef(crate::services::ecam_pci_config::ecam_static()))
}

/// C5: the serial provider node, for endowing the boot domain.
pub fn serial_node() -> Arc<dyn Obj> {
    Arc::new(NodeRef(crate::services::serial::kernel_serial_static()))
}

// ── DMA adapter ────────────────────────────────────────────────────────

pub const DMA_CONTRACT: ContractId = ContractId::of("dma:alloc", &DMA_SURFACE, &DMA_HOOKS);
pub const DMA_ALLOC_PAGE: HookId = HookId::of("alloc_page");
pub const DMA_ALLOC_CONTIG: HookId = HookId::of("alloc_contiguous");
pub const DMA_MAP_MMIO: HookId = HookId::of("map_mmio");
pub const DMA_VIRT_TO_PHYS: HookId = HookId::of("virt_to_phys");

pub const DMA_DOC: &str = "if you call alloc_page, you get a (phys, virt, size) \
buffer owned by this DMA provider; alloc_contiguous(n) is the same for an \
n-page run; map_mmio(paddr, size) maps device MMIO and returns a VA; \
virt_to_phys(vaddr) resolves an existing mapping to its physical address.";

const DMA_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "dma:alloc",
    attrs: &[],
    events: &[],
};

const DMA_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "alloc_page",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64, TypeTag::U64]),
    },
    HookSignature {
        name: "alloc_contiguous",
        params: &[TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64, TypeTag::U64]),
    },
    HookSignature {
        name: "map_mmio",
        params: &[TypeTag::U64, TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "virt_to_phys",
        params: &[TypeTag::U64],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
];

static DMA_CONTRACTS: &[ContractId] = &[DMA_CONTRACT];

impl Obj for KernelDma {
    fn obj_id(&self) -> ObjId {
        DMA_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "svc:dma"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        DMA_CONTRACTS
    }

    fn dispatch(
        &self,
        caller: &CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == DMA_ALLOC_PAGE {
            return dma_alloc(caller, nodes::PHYSMEM_ALLOC_FRAMES, 1);
        }
        if hook == DMA_ALLOC_CONTIG {
            let count = arg_u64(args, 0).unwrap_or(1) as usize;
            return dma_alloc(caller, nodes::PHYSMEM_ALLOC_CONTIG, count);
        }
        if hook == DMA_MAP_MMIO {
            let paddr = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let size = arg_u64(args, 1).ok_or(ObjError::Denied)?;
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] map_mmio paddr=0x");
                SerialPort::put_hex(paddr);
                SerialPort::puts(" size=");
                SerialPort::put_u64(size);
                SerialPort::puts("\n");
            });
            let addrspace = caller
                .resolve_first(nodes::ADDRSPACE_CONTRACT, nodes::ADDRSPACE_MAP)
                .ok_or(ObjError::Denied)?;
            let page_aligned = (size + 4095) & !4095;
            let va = match crate::mm::layout::region_next_down("dma", page_aligned) {
                Some(v) => v,
                None => {
                    dma_trace!({
                        use crate::drivers::serial::SerialPort;
                        SerialPort::puts("[DBG:dma-alloc] map_mmio region_next_down -> None (OutOfMemory)\n");
                    });
                    return Err(ObjError::OutOfMemory);
                }
            };
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] map_mmio va=");
                SerialPort::put_hex(va);
                SerialPort::puts("\n");
            });
            let map_args = Args {
                vals: vec![
                    Value::U64(va),
                    Value::U64(paddr),
                    Value::U64(page_aligned),
                    Value::U64(dma_map_flags()),
                ],
            };
            invoke(caller, addrspace, nodes::ADDRSPACE_CONTRACT, nodes::ADDRSPACE_MAP, &map_args)?;
            return Ok(Reply::Data(vec![Value::U64(va)]));
        }
        if hook == DMA_VIRT_TO_PHYS {
            let addrspace = caller
                .resolve_first(nodes::ADDRSPACE_CONTRACT, nodes::ADDRSPACE_TRANSLATE)
                .ok_or(ObjError::Denied)?;
            return match invoke(
                caller,
                addrspace,
                nodes::ADDRSPACE_CONTRACT,
                nodes::ADDRSPACE_TRANSLATE,
                args,
            )? {
                Reply::Data(vals) => match vals.as_slice() {
                    [Value::U64(phys)] => Ok(Reply::Data(vec![Value::U64(*phys)])),
                    _ => Err(ObjError::Denied),
                },
                _ => Err(ObjError::Denied),
            };
        }
        Err(ObjError::NotSupported)
    }
}

fn arg_u64(args: &Args, i: usize) -> Option<u64> {
    match args.vals.get(i) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

/// The uncached PTE flags a DMA buffer carries, encoded as the raw bits the
/// `mm:address_space` `map` hook decodes (bit0 READ, bit1 WRITE, bit3 NO_CACHE;
/// see `nodes::page_flags`).
fn dma_map_flags() -> u64 {
    (PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE).bits() as u64
}

/// §2.7 graph composition — allocate `count` DMA pages through the caller's
/// endowed `physmem`/`addrspace` capabilities instead of the ambient
/// allocator. Frames come from `PhysMemNode`, the region's base is read back
/// through its `mem:region` capability, and the mapping goes through
/// `AddressSpaceNode::map`. Never panics: every shape mismatch collapses to
/// `ObjError`.
fn dma_alloc(
    caller: &CapabilityTable,
    alloc_hook: HookId,
    count: usize,
) -> Result<Reply, ObjError> {
    let size = (count as u64) * 4096;
    dma_trace!({
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[DBG:dma-alloc] hook=");
        SerialPort::puts(if alloc_hook == nodes::PHYSMEM_ALLOC_FRAMES { "alloc_frames" } else { "alloc_contiguous" });
        SerialPort::puts(" count=");
        SerialPort::put_u64(count as u64);
        SerialPort::puts("\n");
    });

    let physmem = match caller.resolve_first(nodes::PHYSMEM_CONTRACT, nodes::PHYSMEM_ALLOC_FRAMES) {
        Some(id) => id,
        None => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] resolve_first(PHYSMEM) -> None (Denied)\n");
            });
            return Err(ObjError::Denied);
        }
    };
    let addrspace = match caller.resolve_first(nodes::ADDRSPACE_CONTRACT, nodes::ADDRSPACE_MAP) {
        Some(id) => id,
        None => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] resolve_first(ADDRSPACE) -> None (Denied)\n");
            });
            return Err(ObjError::Denied);
        }
    };

    // Reserve the VA window (the "keep the window logic" part).
    let va = match crate::mm::layout::region_next_down("dma", size) {
        Some(v) => v,
        None => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] region_next_down(dma,");
                SerialPort::put_u64(size);
                SerialPort::puts(") -> None (OutOfMemory)\n");
            });
            return Err(ObjError::OutOfMemory);
        }
    };
    dma_trace!({
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[DBG:dma-alloc] va=");
        SerialPort::put_hex(va);
        SerialPort::puts("\n");
    });

    // Allocate the frame(s); `alloc_frames` defaults its count to 1.
    let alloc_args = Args { vals: vec![Value::U64(count as u64)] };
    let reply = match invoke(caller, physmem, nodes::PHYSMEM_CONTRACT, alloc_hook, &alloc_args) {
        Ok(r) => r,
        Err(e) => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] invoke physmem alloc -> Err(OutOfMemory) (pool or frame)\n");
            });
            return Err(e);
        }
    };
    let region_cap_id = match reply {
        Reply::Caps(caps) => match caps.as_slice() {
            [h] => h.id,
            _ => {
                dma_trace!({
                    use crate::drivers::serial::SerialPort;
                    SerialPort::puts("[DBG:dma-alloc] physmem reply caps len != 1 -> OOM\n");
                });
                return Err(ObjError::OutOfMemory);
            }
        },
        _ => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] physmem reply not Caps -> OOM\n");
            });
            return Err(ObjError::OutOfMemory);
        }
    };

    // Read the frame base back through the region's own capability.
    let base = match invoke(
        caller,
        region_cap_id,
        memregion::MEM_REGION_CONTRACT,
        memregion::MEM_REGION_BASE,
        &Args::none(),
    ) {
        Ok(Reply::Data(vals)) => match vals.as_slice() {
            [Value::U64(b)] if *b != 0 => *b,
            [_] => {
                dma_trace!({
                    use crate::drivers::serial::SerialPort;
                    SerialPort::puts("[DBG:dma-alloc] MEM_REGION_BASE -> base=0 (stale wrapper) -> OOM\n");
                });
                return Err(ObjError::OutOfMemory);
            }
            _ => {
                dma_trace!({
                    use crate::drivers::serial::SerialPort;
                    SerialPort::puts("[DBG:dma-alloc] MEM_REGION_BASE shape mismatch -> OOM\n");
                });
                return Err(ObjError::OutOfMemory);
            }
        },
        Ok(_) => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] MEM_REGION_BASE reply not Data -> OOM\n");
            });
            return Err(ObjError::OutOfMemory);
        }
        Err(e) => {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:dma-alloc] MEM_REGION_BASE invoke -> Err(Denied/NoSuchCap) -> OOM\n");
            });
            return Err(e);
        }
    };

    // Map it through the caller's address-space capability, then zero it.
    let map_args = Args {
        vals: vec![
            Value::U64(va),
            Value::U64(base),
            Value::U64(size),
            Value::U64(dma_map_flags()),
        ],
    };
    if let Err(e) = invoke(caller, addrspace, nodes::ADDRSPACE_CONTRACT, nodes::ADDRSPACE_MAP, &map_args) {
        dma_trace!({
            use crate::drivers::serial::SerialPort;
            SerialPort::puts("[DBG:dma-alloc] ADDRSPACE_MAP invoke -> Err(Denied/NoSuchCap) -> OOM\n");
        });
        return Err(e);
    }
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, size as usize) }

    // The region wrapper's sole job was to carry the base through a capability
    // so `alloc_frames` could hand it out without allocating. The caller now
    // owns the frames as raw scalars, so recycle the wrapper (detach = recycle
    // without releasing the backing) or every `alloc_page` permanently drains
    // one entry from the finite wrapper pool.
    let _ = invoke(
        caller,
        region_cap_id,
        memregion::MEM_REGION_CONTRACT,
        memregion::MEM_REGION_DETACH,
        &Args::none(),
    );

    dma_trace!({
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[DBG:dma-alloc] OK phys=");
        SerialPort::put_hex(base);
        SerialPort::puts(" va=");
        SerialPort::put_hex(va);
        SerialPort::puts(" size=");
        SerialPort::put_u64(size);
        SerialPort::puts("\n");
    });

    // Same reply shape as before, so `DmaClient::decode_buffer` is unchanged.
    Ok(Reply::Data(vec![
        Value::U64(base),
        Value::U64(va),
        Value::U64(size),
    ]))
}

// ── PCI config-space adapter ───────────────────────────────────────────

pub const PCI_CONTRACT: ContractId = ContractId::of("pci-config", &PCI_SURFACE, &PCI_HOOKS);
pub const PCI_READ8: HookId = HookId::of("read8");
pub const PCI_READ16: HookId = HookId::of("read16");
pub const PCI_READ32: HookId = HookId::of("read32");
pub const PCI_WRITE8: HookId = HookId::of("write8");
pub const PCI_WRITE16: HookId = HookId::of("write16");
pub const PCI_WRITE32: HookId = HookId::of("write32");

pub const PCI_DOC: &str = "if you call read{8,16,32}(seg, bus, dev, func, off), \
you read the given PCI config-space register and get a U64 value; \
write{8,16,32}(seg, bus, dev, func, off, val) writes the value and replies \
with nothing.";

const PCI_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "pci-config",
    attrs: &[],
    events: &[],
};

const PCI_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "read8",
        params: &[
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
        ],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "read16",
        params: &[
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
        ],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "read32",
        params: &[
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
        ],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "write8",
        params: &[
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
        ],
        reply: ReplyTag::None,
    },
    HookSignature {
        name: "write16",
        params: &[
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
        ],
        reply: ReplyTag::None,
    },
    HookSignature {
        name: "write32",
        params: &[
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
            TypeTag::U64,
        ],
        reply: ReplyTag::None,
    },
];

static PCI_CONTRACTS: &[ContractId] = &[PCI_CONTRACT];

impl Obj for EcamPciConfig {
    fn obj_id(&self) -> ObjId {
        PCI_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "svc:pci-config"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        PCI_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        let seg = arg_u64(args, 0).unwrap_or(0) as u16;
        let bus = arg_u64(args, 1).unwrap_or(0) as u8;
        let dev = arg_u64(args, 2).unwrap_or(0) as u8;
        let func = arg_u64(args, 3).unwrap_or(0) as u8;
        let off = arg_u64(args, 4).unwrap_or(0) as u16;

        if hook == PCI_READ8 {
            return Ok(Reply::Data(vec![Value::U64(self.read8(seg, bus, dev, func, off) as u64)]));
        }
        if hook == PCI_READ16 {
            return Ok(Reply::Data(vec![Value::U64(self.read16(seg, bus, dev, func, off) as u64)]));
        }
        if hook == PCI_READ32 {
            return Ok(Reply::Data(vec![Value::U64(self.read32(seg, bus, dev, func, off) as u64)]));
        }
        if hook == PCI_WRITE8 {
            let val = arg_u64(args, 5).unwrap_or(0) as u8;
            self.write8(seg, bus, dev, func, off, val);
            return Ok(Reply::None);
        }
        if hook == PCI_WRITE16 {
            let val = arg_u64(args, 5).unwrap_or(0) as u16;
            self.write16(seg, bus, dev, func, off, val);
            return Ok(Reply::None);
        }
        if hook == PCI_WRITE32 {
            let val = arg_u64(args, 5).unwrap_or(0) as u32;
            self.write32(seg, bus, dev, func, off, val);
            return Ok(Reply::None);
        }
        Err(ObjError::NotSupported)
    }
}

// ── Serial console adapter ─────────────────────────────────────────────

pub const SERIAL_CONTRACT: ContractId = ContractId::of("serial-console", &SERIAL_SURFACE, &SERIAL_HOOKS);
pub const SERIAL_PUTC: HookId = HookId::of("putc");
pub const SERIAL_PUTS: HookId = HookId::of("puts");

pub const SERIAL_DOC: &str = "if you call putc(c), one character is written to \
the console and nothing is replied; puts(s) writes the given string.";

const SERIAL_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "serial-console",
    attrs: &[],
    events: &[],
};

const SERIAL_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "putc",
        params: &[TypeTag::U64],
        reply: ReplyTag::None,
    },
    HookSignature {
        name: "puts",
        params: &[TypeTag::Str],
        reply: ReplyTag::None,
    },
];

static SERIAL_CONTRACTS: &[ContractId] = &[SERIAL_CONTRACT];

impl Obj for KernelSerial {
    fn obj_id(&self) -> ObjId {
        SERIAL_OBJ_ID
    }

    fn kind(&self) -> &'static str {
        "svc:serial"
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        None
    }

    fn contracts(&self) -> &'static [ContractId] {
        SERIAL_CONTRACTS
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == SERIAL_PUTC {
            if let Some(Value::U64(c)) = args.vals.first() {
                self.putc(*c as u8);
            }
            return Ok(Reply::None);
        }
        if hook == SERIAL_PUTS {
            for v in &args.vals {
                match v {
                    // `&'static str` labels (the classic form).
                    Value::Str(s) => self.puts(s),
                    // Runtime `&str` payloads marshalled by `SerialClient`
                    // (`Value::Str` cannot carry a non-static string).
                    Value::Buf(b) => self.puts(core::str::from_utf8(b).unwrap_or("")),
                    _ => {}
                }
            }
            return Ok(Reply::None);
        }
        Err(ObjError::NotSupported)
    }
}

// ── Contract definitions (§7.2.4, §7.8) ──────────────────────────────────
//
// Each provider node advertises a `ContractId` (its `contracts()`); the
// registry carries the full `Contract` — id, name, surface, hooks, and doc —
// so a driver holding the registry capability can ask "what does this
// contract promise?" and get the definition back. The ids are recomputed
// from the same statics they were declared with, so `def.id` and
// `ContractId::of(name, surface, hooks)` agree by construction.

static DMA_CONTRACT_DEF: Contract = Contract {
    id: DMA_CONTRACT,
    name: "dma:alloc",
    surface: &DMA_SURFACE,
    hooks: DMA_HOOKS,
    doc: DMA_DOC,
};

static PCI_CONTRACT_DEF: Contract = Contract {
    id: PCI_CONTRACT,
    name: "pci-config",
    surface: &PCI_SURFACE,
    hooks: PCI_HOOKS,
    doc: PCI_DOC,
};

static SERIAL_CONTRACT_DEF: Contract = Contract {
    id: SERIAL_CONTRACT,
    name: "serial-console",
    surface: &SERIAL_SURFACE,
    hooks: SERIAL_HOOKS,
    doc: SERIAL_DOC,
};

// ── physical-world contracts (`nodes.rs`) ────────────────────────────────
//
// The five physical-world contracts are declared in `obj/nodes.rs` (content-addressed
// `ContractId`s like `PHYSMEM_CONTRACT`), but their `SurfaceDesc`/hooks statics
// there are private. The canonical `Contract` defs therefore re-declare the
// same surface/hooks here, and pin `id` to the `nodes.rs` consts — the
// authoritative ids the nodes advertise in `contracts()` — so the registry's
// `lookup` by a node's contract id resolves to the matching def (I10).
// The surface/hooks below MUST stay byte-identical to `nodes.rs`.

const PHYSMEM_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "physmem:allocation",
    attrs: &[SurfaceAttr { name: "total_frames", ty: TypeTag::U64 }],
    events: &[],
};

const PHYSMEM_HOOKS: &[HookSignature] = &[
    HookSignature { name: "alloc_frames", params: &[TypeTag::U64], reply: ReplyTag::Caps },
    HookSignature { name: "alloc_contiguous", params: &[TypeTag::U64], reply: ReplyTag::Caps },
    HookSignature { name: "free", params: &[TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "reserve", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "stats", params: &[], reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64]) },
];

const HEAP_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "heap:allocation",
    attrs: &[SurfaceAttr { name: "arena", ty: TypeTag::U64 }],
    events: &[],
};

const HEAP_HOOKS: &[HookSignature] = &[
    HookSignature { name: "alloc", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::Caps },
    HookSignature { name: "free", params: &[TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "stats", params: &[], reply: ReplyTag::None },
];

const ADDRSPACE_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "mm:address_space",
    attrs: &[SurfaceAttr { name: "root", ty: TypeTag::U64 }],
    events: &[],
};

const ADDRSPACE_HOOKS: &[HookSignature] = &[
    HookSignature { name: "map", params: &[TypeTag::U64, TypeTag::U64, TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "unmap", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "protect", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "shootdown", params: &[], reply: ReplyTag::None },
    HookSignature { name: "translate", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "root", params: &[], reply: ReplyTag::Data(&[TypeTag::U64]) },
];

const CPU_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "smp:cpu",
    attrs: &[SurfaceAttr { name: "cpus", ty: TypeTag::U64 }],
    events: &[],
};

const CPU_HOOKS: &[HookSignature] = &[
    HookSignature { name: "wake", params: &[TypeTag::U64], reply: ReplyTag::Data(&[TypeTag::U64]) },
    HookSignature { name: "ipi", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "shootdown", params: &[], reply: ReplyTag::None },
    HookSignature { name: "stats", params: &[], reply: ReplyTag::Data(&[TypeTag::U64, TypeTag::U64, TypeTag::U64]) },
];

const IRQ_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "irq:vector",
    attrs: &[SurfaceAttr { name: "vectors", ty: TypeTag::U64 }],
    events: &[],
};

const IRQ_HOOKS: &[HookSignature] = &[
    HookSignature { name: "register_handler", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "unregister", params: &[TypeTag::U64], reply: ReplyTag::None },
    HookSignature { name: "ack", params: &[], reply: ReplyTag::None },
    HookSignature { name: "set_enabled", params: &[TypeTag::U64, TypeTag::U64], reply: ReplyTag::None },
];

static PHYSMEM_CONTRACT_DEF: Contract = Contract {
    id: nodes::PHYSMEM_CONTRACT,
    name: "physmem:allocation",
    surface: &PHYSMEM_SURFACE,
    hooks: PHYSMEM_HOOKS,
    doc: nodes::PHYSMEM_DOC,
};

static HEAP_CONTRACT_DEF: Contract = Contract {
    id: nodes::HEAP_CONTRACT,
    name: "heap:allocation",
    surface: &HEAP_SURFACE,
    hooks: HEAP_HOOKS,
    doc: nodes::HEAP_DOC,
};

static ADDRSPACE_CONTRACT_DEF: Contract = Contract {
    id: nodes::ADDRSPACE_CONTRACT,
    name: "mm:address_space",
    surface: &ADDRSPACE_SURFACE,
    hooks: ADDRSPACE_HOOKS,
    doc: nodes::ADDRSPACE_DOC,
};

static CPU_CONTRACT_DEF: Contract = Contract {
    id: nodes::CPU_CONTRACT,
    name: "smp:cpu",
    surface: &CPU_SURFACE,
    hooks: CPU_HOOKS,
    doc: nodes::CPU_DOC,
};

static IRQ_CONTRACT_DEF: Contract = Contract {
    id: nodes::IRQ_CONTRACT,
    name: "irq:vector",
    surface: &IRQ_SURFACE,
    hooks: IRQ_HOOKS,
    doc: nodes::IRQ_DOC,
};

/// The canonical definitions of the five physical-world contracts (§7.10).
/// The bootstrap agent registers these through the registry capability.
static PHYSICAL_CONTRACT_DEFS: &[&Contract] = &[
    &PHYSMEM_CONTRACT_DEF,
    &HEAP_CONTRACT_DEF,
    &ADDRSPACE_CONTRACT_DEF,
    &CPU_CONTRACT_DEF,
    &IRQ_CONTRACT_DEF,
];

/// The canonical definitions of the five physical-world contracts (§7.10).
pub fn physical_contract_defs() -> &'static [&'static Contract] {
    PHYSICAL_CONTRACT_DEFS
}

/// The canonical definition of the DMA contract (§7.8).
pub fn dma_contract_def() -> &'static Contract {
    &DMA_CONTRACT_DEF
}

/// The canonical definition of the PCI-config contract (§7.8).
pub fn pci_contract_def() -> &'static Contract {
    &PCI_CONTRACT_DEF
}

/// The canonical definition of the serial-console contract (§7.8).
pub fn serial_contract_def() -> &'static Contract {
    &SERIAL_CONTRACT_DEF
}

/// Find the canonical contract definition by contract name (§7.8). The
/// registry node's `register` hook consults this, so a discovered `def` is
/// always one the kernel trusts enough to seed — a remote domain cannot
/// inject an arbitrary tuple.
pub fn contract_def(name: &str) -> Option<&'static Contract> {
    if name == DMA_CONTRACT_DEF.name {
        Some(&DMA_CONTRACT_DEF)
    } else if name == PCI_CONTRACT_DEF.name {
        Some(&PCI_CONTRACT_DEF)
    } else if name == SERIAL_CONTRACT_DEF.name {
        Some(&SERIAL_CONTRACT_DEF)
    } else if name == PHYSMEM_CONTRACT_DEF.name {
        Some(&PHYSMEM_CONTRACT_DEF)
    } else if name == HEAP_CONTRACT_DEF.name {
        Some(&HEAP_CONTRACT_DEF)
    } else if name == ADDRSPACE_CONTRACT_DEF.name {
        Some(&ADDRSPACE_CONTRACT_DEF)
    } else if name == CPU_CONTRACT_DEF.name {
        Some(&CPU_CONTRACT_DEF)
    } else if name == IRQ_CONTRACT_DEF.name {
        Some(&IRQ_CONTRACT_DEF)
    } else if name == fs::block_contract_def().name {
        Some(fs::block_contract_def())
    } else if name == fs::block_family_contract_def().name {
        Some(fs::block_family_contract_def())
    } else if name == fs::mount_contract_def().name {
        Some(fs::mount_contract_def())
    } else if name == fs::dir_contract_def().name {
        Some(fs::dir_contract_def())
    } else if name == fs::file_contract_def().name {
        Some(fs::file_contract_def())
    } else if name == devices::pci_forest_contract_def().name {
        Some(devices::pci_forest_contract_def())
    } else if name == devices::input_family_contract_def().name {
        Some(devices::input_family_contract_def())
    } else if name == devices::audio_family_contract_def().name {
        Some(devices::audio_family_contract_def())
    } else if name == memregion::MEM_REGION_CONTRACT_DEF.name {
        Some(&memregion::MEM_REGION_CONTRACT_DEF)
    } else {
        None
    }
}