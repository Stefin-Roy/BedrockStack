//! C4 — `Obj` adapters for the kernel service providers (§7.2.1, §7.11.4).
//!
//! The Boot domain holds a capability to each provider and reaches it through
//! the `invoke` entry (§7.9) — full invoke form, no downcast fast path. Each
//! adapter wraps the existing `Capability` provider (its `Capability` impl is
//! untouched) by implementing `Obj`: `dispatch` delegates to the same trait
//! methods the object already uses, so a hook call reaches the singleton's
//! real internal state.
//!
//! All adapters are arch-neutral.

use alloc::sync::Arc;
use alloc::vec;

use crate::services::dma::DmaAllocator;
use crate::services::ecam_pci_config::EcamPciConfig;
use crate::services::pci_config::PciConfigSpace;
use crate::services::serial::SerialConsole;
use crate::services::serial::KernelSerial;
use crate::services::dma::KernelDma;

use super::cap_handle::RevocationPolicy;
use super::contract::ContractId;
use super::hook::HookId;
use super::surface::SurfaceDesc;
use super::table::CapabilityTable;
use super::{Args, Obj, ObjError, ObjId, Reply, Value};

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
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        self.0.dispatch(caller, hook, args)
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

pub const DMA_CONTRACT: ContractId = ContractId::from_name("dma:alloc");
pub const DMA_ALLOC_PAGE: HookId = HookId::of("alloc_page");
pub const DMA_ALLOC_CONTIG: HookId = HookId::of("alloc_contiguous");
pub const DMA_MAP_MMIO: HookId = HookId::of("map_mmio");
pub const DMA_VIRT_TO_PHYS: HookId = HookId::of("virt_to_phys");

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
        _caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == DMA_ALLOC_PAGE {
            return match self.alloc_page() {
                Some(buf) => reply_dma_buffer(&buf),
                None => Err(ObjError::OutOfMemory),
            };
        }
        if hook == DMA_ALLOC_CONTIG {
            let count = arg_u64(args, 0).unwrap_or(1) as usize;
            return match self.alloc_contiguous(count) {
                Some(buf) => reply_dma_buffer(&buf),
                None => Err(ObjError::OutOfMemory),
            };
        }
        if hook == DMA_MAP_MMIO {
            let paddr = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let size = arg_u64(args, 1).ok_or(ObjError::Denied)?;
            return match self.map_mmio(paddr, size) {
                Ok(va) => Ok(Reply::Data(vec![Value::U64(va)])),
                Err(_) => Err(ObjError::OutOfMemory),
            };
        }
        if hook == DMA_VIRT_TO_PHYS {
            let vaddr = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            return match self.virt_to_phys(vaddr) {
                Some(phys) => Ok(Reply::Data(vec![Value::U64(phys)])),
                None => Err(ObjError::Denied),
            };
        }
        Err(ObjError::NotSupported)
    }
}

fn reply_dma_buffer(buf: &crate::services::dma::DmaBuffer) -> Result<Reply, ObjError> {
    Ok(Reply::Data(vec![
        Value::U64(buf.phys),
        Value::U64(buf.virt),
        Value::U64(buf.size as u64),
    ]))
}

fn arg_u64(args: &Args, i: usize) -> Option<u64> {
    match args.vals.get(i) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

// ── PCI config-space adapter ───────────────────────────────────────────

pub const PCI_CONTRACT: ContractId = ContractId::from_name("pci-config");
pub const PCI_READ8: HookId = HookId::of("read8");
pub const PCI_READ16: HookId = HookId::of("read16");
pub const PCI_READ32: HookId = HookId::of("read32");
pub const PCI_WRITE8: HookId = HookId::of("write8");
pub const PCI_WRITE16: HookId = HookId::of("write16");
pub const PCI_WRITE32: HookId = HookId::of("write32");

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

pub const SERIAL_CONTRACT: ContractId = ContractId::from_name("serial-console");
pub const SERIAL_PUTC: HookId = HookId::of("putc");
pub const SERIAL_PUTS: HookId = HookId::of("puts");

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
                if let Value::Str(s) = v {
                    self.puts(s);
                }
            }
            return Ok(Reply::None);
        }
        Err(ObjError::NotSupported)
    }
}