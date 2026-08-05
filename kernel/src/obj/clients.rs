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
    PCI_READ16, PCI_READ32, PCI_READ8, PCI_WRITE16, PCI_WRITE32, PCI_WRITE8, SERIAL_PUTC,
    SERIAL_PUTS,
};
use super::bootstrap::{boot_domain, boot_endowment};
use super::cap_handle::{CapHandle, CapId, HandleState};
use super::driver::{driver_domain, driver_endowment};
use super::hook::HookId;
use super::nodes;
use super::rights::{CapRights, ContractRights, Rights};
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

/// A capability-mediated console writer. `Copy`: it owns the table reference
/// (the boot table — the console is a boot-domain service) plus the `CapId`
/// naming the serial provider node, so it can be threaded wherever raw console
/// writes once went (§7.7.1, §7.9). Every write is fire-and-forget: the hooks
/// reply `Reply::None` and any error is discarded, matching the current console
/// semantics.
#[derive(Clone, Copy)]
pub struct SerialClient {
    table: &'static CapabilityTable,
    cap: CapId,
}

impl SerialClient {
    /// Bind a serial client to an explicit domain's table + CapId (C8: clients
    /// are constructed per-domain, never from a global).
    pub fn new(table: &'static CapabilityTable, cap: CapId) -> Self {
        SerialClient { table, cap }
    }

    /// The Boot domain's serial capability (§5.4). The console is a
    /// boot-domain service — there is no driver-domain serial endowment — so
    /// this is the only constructor.
    pub fn boot_serial() -> Self {
        Self::new(&boot_domain().table, boot_endowment().serial)
    }

    /// Invoke a serial hook and drop the (always `Reply::None`) reply. Errors
    /// are discarded: console output is best-effort.
    fn call(&self, hook: HookId, args: &Args) {
        let _ = invoke(self.table, self.cap, adapters::SERIAL_CONTRACT, hook, args);
    }

    /// Write `s` to the console (fire-and-forget). The string crosses the
    /// capability as a buffer, so runtime `&str`s are supported.
    pub fn puts(&self, s: &str) {
        let args = Args { vals: Vec::from([Value::Buf(s.as_bytes().to_vec())]) };
        self.call(SERIAL_PUTS, &args);
    }

    /// Write one byte to the console (fire-and-forget).
    pub fn putc(&self, c: u8) {
        let args = Args { vals: Vec::from([Value::U64(c as u64)]) };
        self.call(SERIAL_PUTC, &args);
    }

    /// Write `v` as lowercase hex with no prefix (fire-and-forget). Mirrors
    /// `SerialPort::put_hex` output.
    pub fn put_hex(&self, v: u64) {
        let mut buf = [0u8; 16];
        let s = hex_str(v, &mut buf);
        self.puts(s);
    }

    /// Write `v` in decimal with no prefix (fire-and-forget). Mirrors
    /// `SerialPort::put_u64` output.
    pub fn put_u64(&self, v: u64) {
        let mut buf = [0u8; 20];
        let s = dec_str(v, &mut buf);
        self.puts(s);
    }
}

/// Format `v` as lowercase hex into `buf` (needs 16 bytes) and return the
/// slice, mirroring `common::serial::SerialPort::put_hex`.
fn hex_str<'a>(v: u64, buf: &'a mut [u8; 16]) -> &'a str {
    let mut val = v;
    if val == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("");
    }
    let mut i = 16;
    while val > 0 {
        i -= 1;
        let digit = (val & 0xF) as u8;
        buf[i] = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
        val >>= 4;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("")
}

/// Format `v` in decimal into `buf` (needs 20 bytes) and return the slice,
/// mirroring `common::serial::SerialPort::put_u64`.
fn dec_str<'a>(v: u64, buf: &'a mut [u8; 20]) -> &'a str {
    let mut val = v;
    if val == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("");
    }
    let mut i = 20;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("")
}

/// A capability-mediated interrupt-family client. `Copy`: it owns the table
/// reference plus the `CapId` naming the `irq:vector` root, so it can be
/// threaded wherever the IRQ family was once reached ambiently (§7.7.1, §7.9).
/// Handlers reach `register_handler` *by capability*: each `register`
/// materializes a kernel `IrqHandlerNode` over the `fn()` and hands its `CapId`
/// to the hook — never a raw caller-supplied address.
#[derive(Clone, Copy)]
pub struct IrqClient {
    table: &'static CapabilityTable,
    cap: CapId,
}

impl IrqClient {
    /// Bind an IRQ client to an explicit domain's table + CapId (C8).
    pub fn new(table: &'static CapabilityTable, cap: CapId) -> Self {
        IrqClient { table, cap }
    }

    /// The Boot domain's IRQ-family capability (§5.4). Valid once bootstrap has
    /// run.
    pub fn boot_irq() -> Self {
        Self::new(&boot_domain().table, boot_endowment().irq)
    }

    /// The first driver domain's IRQ-family capability (§6.2). The irq cap the
    /// driver table holds carries READ|WRITE|CALL, so `register`/`ack` (CALL)
    /// and `unregister`/`set_enabled` (WRITE) all pass PERMIT.
    pub fn driver_irq() -> Self {
        Self::new(&driver_domain().table, driver_endowment().irq)
    }

    /// Invoke an IRQ hook and collapse the (always `Reply::None`) reply to
    /// `Ok(())`; errors are preserved.
    fn call(&self, hook: HookId, args: &Args) -> Result<(), ObjError> {
        match invoke(self.table, self.cap, nodes::IRQ_CONTRACT, hook, args) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Bind `handler` to `vector`, or to an MSI-allocated free device vector
    /// when `vector` is `None` (the vector may be omitted; auto-allocation
    /// happens inside the Irq node). A fresh [`nodes::IrqHandlerNode`] is
    /// materialized over the `fn()` and inserted into this client's table; its
    /// `CapId` is passed to the `register_handler` hook. The hook replies the
    /// assigned vector, so auto-allocation is observable: on
    /// `Reply::Data([Value::U64(v)])` this returns `Ok(v as u8)`. Any other
    /// reply shape collapses to `Err` (a shape mismatch maps to `Denied`, a
    /// non-`Data` reply to `NotSupported`); invoke errors are preserved.
    pub fn register(&self, vector: Option<u8>, handler: fn()) -> Result<u8, ObjError> {
        let handler_cap = self.table.insert_handle(CapHandle {
            id: CapId(0),
            node: nodes::handler_node(handler),
            rights: CapRights::new(Rights::INVOKE, ContractRights::empty()),
            state: HandleState::Live,
        });
        let args = Args {
            vals: Vec::from([Value::U64(vector.unwrap_or(0) as u64), Value::U64(handler_cap.0)]),
        };
        match invoke(self.table, self.cap, nodes::IRQ_CONTRACT, nodes::IRQ_REGISTER, &args) {
            Ok(Reply::Data(vals)) => match vals.first() {
                Some(Value::U64(v)) => Ok(*v as u8),
                _ => Err(ObjError::Denied),
            },
            Ok(_) => Err(ObjError::NotSupported),
            Err(e) => Err(e),
        }
    }

    /// Unbind the handler on `vector` and release its device vector (§7.10.5).
    pub fn unregister(&self, vector: u8) -> Result<(), ObjError> {
        let args = Args { vals: Vec::from([Value::U64(vector as u64)]) };
        self.call(nodes::IRQ_UNREGISTER, &args)
    }

    /// Send end-of-interrupt (§7.10.5).
    pub fn ack(&self) -> Result<(), ObjError> {
        self.call(nodes::IRQ_ACK, &Args::none())
    }

    /// Enable or disable the handler on `vector`.
    pub fn set_enabled(&self, vector: u8, on: bool) -> Result<(), ObjError> {
        let args = Args {
            vals: Vec::from([Value::U64(vector as u64), Value::U64(on as u64)]),
        };
        self.call(nodes::IRQ_SET_ENABLED, &args)
    }
}