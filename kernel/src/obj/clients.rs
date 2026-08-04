//! C6 — capability-mediated client wrappers for the endowed providers (§7.9).
//!
//! A caller no longer touches the DMA provider ambiently. Instead it holds a
//! `CapId` naming the DMA node inside the boot table and reaches the
//! provider only through `crate::obj::invoke` (§7.7.1: "the providers are
//! reachable, but only through the table"). Each `DmaClient` marshals one DMA
//! operation through that capability, decoding the `Reply::Data` payload back
//! into the existing `DmaBuffer` / scalar shapes so the driver call sites keep
//! their readability. Arch-neutral.

extern crate alloc;

use alloc::vec::Vec;

use super::adapters::{
    self, DMA_ALLOC_CONTIG, DMA_ALLOC_PAGE, DMA_MAP_MMIO, DMA_VIRT_TO_PHYS, PCI_CONTRACT,
    PCI_READ16, PCI_READ32, PCI_READ8, PCI_WRITE16, PCI_WRITE32, PCI_WRITE8,
};
use super::bootstrap::{boot_domain, boot_endowment};
use super::cap_handle::CapId;
use super::driver::{driver_domain, driver_endowment};
use super::hook::HookId;
use super::table::CapabilityTable;
use super::{invoke, Args, ObjError, Reply, Value};
use crate::services::dma::DmaBuffer;

macro_rules! dma_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "dma_trace")]
        $($arg)*
    };
}

/// Human-readable name for the four DMA hooks (`dma_trace` diagnostics).
fn dma_hook_name(h: u64) -> &'static str {
    if h == adapters::DMA_ALLOC_PAGE.0 {
        "alloc_page"
    } else if h == adapters::DMA_ALLOC_CONTIG.0 {
        "alloc_contiguous"
    } else if h == adapters::DMA_MAP_MMIO.0 {
        "map_mmio"
    } else if h == adapters::DMA_VIRT_TO_PHYS.0 {
        "virt_to_phys"
    } else {
        "?"
    }
}

/// A capability-mediated DMA allocator. `Copy`: it owns the table reference
/// (the boot table, or the driver domain's disjoint table) plus the `CapId`
/// naming the DMA node, so it can be threaded wherever a `&dyn DmaAllocator`
/// once was.
#[derive(Clone, Copy)]
pub struct DmaClient {
    table: &'static CapabilityTable,
    cap: CapId,
}

impl DmaClient {
    /// Bind a DMA client to an explicit domain's table + CapId (C8: clients
    /// are constructed per-domain, never from a global).
    pub fn new(table: &'static CapabilityTable, cap: CapId) -> Self {
        DmaClient { table, cap }
    }

    /// The Boot domain's DMA capability (§5.4). Valid once bootstrap has run.
    pub fn boot_dma() -> Self {
        Self::new(&boot_domain().table, boot_endowment().dma)
    }

    /// The first driver domain's DMA capability (§6.2). The device sweep binds
    /// its allocators to this disjoint table, so a boot-only cap is out of
    /// reach (§8.14).
    pub fn driver_dma() -> Self {
        Self::new(&driver_domain().table, driver_endowment().dma)
    }

    /// Invoke the DMA hook and decode a `Reply::Data([Value::U64...])` payload.
    /// Any shape mismatch collapses to `Err`.
    fn call(&self, hook: HookId, args: &Args) -> Result<Vec<u64>, ObjError> {
        match invoke(self.table, self.cap, adapters::DMA_CONTRACT, hook, args) {
            Ok(Reply::Data(vals)) => {
                let mut out = Vec::with_capacity(vals.len());
                for v in vals {
                    match v {
                        Value::U64(u) => out.push(u),
                        _ => return Err(ObjError::Denied),
                    }
                }
                Ok(out)
            }
            Ok(_) => Err(ObjError::NotSupported),
            Err(e) => {
                dma_trace!({
                    use crate::drivers::serial::SerialPort;
                    SerialPort::puts("[DBG:dma-cap] dma hook '");
                    SerialPort::puts(dma_hook_name(hook.0));
                    SerialPort::puts("' invoke failed: ");
                    SerialPort::puts(match e {
                        ObjError::NoSuchCap => "NoSuchCap",
                        ObjError::Denied => "Denied",
                        ObjError::Revoked => "Revoked",
                        ObjError::OutOfMemory => "OutOfMemory",
                        ObjError::NotSupported => "NotSupported",
                        ObjError::Exhausted => "Exhausted",
                        ObjError::Disowned => "Disowned",
                        ObjError::NoAmplification => "NoAmplification",
                        ObjError::MintAuthorityGone => "MintAuthorityGone",
                        ObjError::ContractCollision => "ContractCollision",
                    });
                    SerialPort::puts("\n");
                });
                Err(e)
            }
        }
    }

    pub fn alloc_page(&self) -> Option<DmaBuffer> {
        self.decode_buffer(DMA_ALLOC_PAGE, &Args::none())
    }

    pub fn alloc_contiguous(&self, count: usize) -> Option<DmaBuffer> {
        let args = Args { vals: Vec::from([Value::U64(count as u64)]) };
        self.decode_buffer(DMA_ALLOC_CONTIG, &args)
    }

    pub fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        let args = Args { vals: Vec::from([Value::U64(paddr), Value::U64(size)]) };
        match self.call(DMA_MAP_MMIO, &args) {
            Ok(v) if v.len() == 1 => Ok(v[0]),
            _ => Err("DMA: map_mmio failed"),
        }
    }

    pub fn virt_to_phys(&self, vaddr: u64) -> Option<u64> {
        let args = Args { vals: Vec::from([Value::U64(vaddr)]) };
        match self.call(DMA_VIRT_TO_PHYS, &args) {
            Ok(v) if v.len() == 1 => Some(v[0]),
            _ => None,
        }
    }

    /// A buffer reply is `[phys, virt, size]` (§ adapters).
    fn decode_buffer(&self, hook: HookId, args: &Args) -> Option<DmaBuffer> {
        match self.call(hook, args) {
            Ok(v) if v.len() == 3 => Some(DmaBuffer {
                phys: v[0],
                virt: v[1],
                size: v[2] as usize,
            }),
            _ => None,
        }
    }
}

/// A capability-mediated PCI config-space reader. `Copy`: it owns the table
/// reference plus the `CapId` naming the PCI config node, so it can be
/// threaded wherever a `&dyn PciConfigSpace` once was (§7.7.1, §7.9).
#[derive(Clone, Copy)]
pub struct PciCfgClient {
    table: &'static CapabilityTable,
    cap: CapId,
}

impl PciCfgClient {
    /// Bind a PCI-config client to an explicit domain's table + CapId (C8).
    pub fn new(table: &'static CapabilityTable, cap: CapId) -> Self {
        PciCfgClient { table, cap }
    }

    /// The Boot domain's PCI-config capability (§5.4). Valid once bootstrap
    /// has run (PCI init happens later, inside `Kernel::run()`).
    pub fn boot_pci() -> Self {
        Self::new(&boot_domain().table, boot_endowment().pci_cfg)
    }

    /// The first driver domain's PCI-config capability (§6.2). The PCI
    /// enumeration and device sweep resolve reads/writes through this disjoint
    /// table (§8.14).
    pub fn driver_pci() -> Self {
        Self::new(&driver_domain().table, driver_endowment().pci_cfg)
    }

    /// Invoke a PCI hook and decode the single `Value::U64` reply. Writes
    /// reply `Reply::None`, so they collapse to `None` here and are discarded
    /// by the write methods. Any shape mismatch collapses to `None`.
    fn call(&self, hook: HookId, args: &Args) -> Option<u64> {
        match invoke(self.table, self.cap, PCI_CONTRACT, hook, args) {
            Ok(Reply::Data(vals)) => match vals.as_slice() {
                [Value::U64(v)] => Some(*v),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn read8(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u8 {
        self.call(PCI_READ8, &read_args(seg, bus, dev, func, off)).unwrap_or(0) as u8
    }

    pub fn read16(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u16 {
        self.call(PCI_READ16, &read_args(seg, bus, dev, func, off)).unwrap_or(0) as u16
    }

    pub fn read32(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u32 {
        self.call(PCI_READ32, &read_args(seg, bus, dev, func, off)).unwrap_or(0) as u32
    }

    pub fn write8(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u8) {
        let _ = self.call(PCI_WRITE8, &write_args(seg, bus, dev, func, off, val as u64));
    }

    pub fn write16(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u16) {
        let _ = self.call(PCI_WRITE16, &write_args(seg, bus, dev, func, off, val as u64));
    }

    pub fn write32(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u32) {
        let _ = self.call(PCI_WRITE32, &write_args(seg, bus, dev, func, off, val as u64));
    }
}

/// Argument prefix shared by every PCI hook: `[seg, bus, dev, func, off]`
/// (§ adapters).
fn read_args(seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> Args {
    Args {
        vals: Vec::from([
            Value::U64(seg as u64),
            Value::U64(bus as u64),
            Value::U64(dev as u64),
            Value::U64(func as u64),
            Value::U64(off as u64),
        ]),
    }
}

/// `[seg, bus, dev, func, off, val]` for the write hooks.
fn write_args(seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u64) -> Args {
    Args {
        vals: Vec::from([
            Value::U64(seg as u64),
            Value::U64(bus as u64),
            Value::U64(dev as u64),
            Value::U64(func as u64),
            Value::U64(off as u64),
            Value::U64(val),
        ]),
    }
}