//! P3 — The physical world as nodes (§7.10).
//!
//! Wraps the kernel's own allocators and service providers so that "core
//! memory" is a set of capability-reachable nodes, not an ambient backroom:
//! the frame pool, the heap, the page tables, the CPU family, and the
//! interrupt family. Each node implements [`Obj`] with a stable `ObjId`, a
//! content-addressed contract (mirroring `adapters.rs`), and real hook bodies
//! that reach the original module state (`get_phys_allocator_mut`, the
//! captured page-table root, and the `&'static dyn` service references).
//!
//! ## Allocation-failure contract (§Phase P3)
//!
//! The abort-on-OOM behaviour is reserved for the bootstrap seed; every hook
//! here treats OOM as a *recoverable* `Result` — a failed allocation through
//! a node hook returns [`ObjError::OutOfMemory`], never a panic, and never an
//! assert. Likewise the *no-alloc-in-alloc-hooks* rule: a hook that hands out
//! a region never allocates to *construct* the returned `MemRegion` capability
//! — it pops a pre-built wrapper from `mem_region_pool` (see `memregion.rs`).
//!
//! ## The `free` hook — real backing release
//!
//! The provider `free(region)` hooks take the region's `CapId` from the
//! caller's table, verify it names a `mem:region` node, and delegate to the
//! region's own `free` hook, which returns the backing to its allocator
//! (`BitmapAllocator::free` per frame, or heap `dealloc` with the stored
//! layout) and recycles the pooled wrapper (§ `memregion.rs`). Freeing is thus
//! revoking the capability, and the backing release is safe against double
//! frees (the region zeroes its identity after releasing).

use alloc::alloc::{alloc as alloc_bytes, Layout};
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Once;

use crate::mm::heap;
use crate::mm::vmm::{PageFlags, Vmm};
use crate::services::cpu::CpuManager;
use crate::services::interrupts::InterruptManager;
use crate::services::msi::MsiAllocator;
use crate::services::KernelServices;

use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract::{ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::memregion::{
    MemRegionNode, RegionKind, mem_region_pool, MEM_REGION_CONTRACT, MEM_REGION_FREE,
};
use super::rights::{CapRights, ContractRights, Rights};
use super::store::object_store;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::table::CapabilityTable;
use super::{Args, Obj, ObjError, ObjId, Reply, Value, invoke};

macro_rules! dma_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "dma_trace")]
        $($arg)*
    };
}

// ── Id space (above the infra/`0x10_xxxx` adapter range) ───────────────
//
// Each family root carries its stable `ObjId` as an associated const
// `OBJ_ID`; children are dynamic (`CPU_CHILD_ID_BASE`/`IRQ_CHILD_ID_BASE`).

/// Dynamic per-CPU child ids: `0x11_1000 + cpu_id`.
pub const CPU_CHILD_ID_BASE: u64 = 0x11_1000;
/// Dynamic per-vector child ids: `0x11_2000 + vector`.
pub const IRQ_CHILD_ID_BASE: u64 = 0x11_2000;

/// Vector used for cross-CPU TLB shootdown. Mirrors the x86_64 APIC's
/// `IPI_TLB_SHOOTDOWN` (50); the RISC-V CPU backend ignores the vector ("`_vector`")
/// and only carries the hart mask, so the value is arch-neutral to invoke.
const TLB_SHOOTDOWN_VECTOR: u8 = 50;

// ── PhysMem — the frame pool (§7.10.1) ─────────────────────────────────

pub const PHYSMEM_CONTRACT: ContractId =
    ContractId::of("physmem:allocation", &PHYSMEM_SURFACE, &PHYSMEM_HOOKS);
pub const PHYSMEM_ALLOC_FRAMES: HookId = HookId::of("alloc_frames");
pub const PHYSMEM_ALLOC_CONTIG: HookId = HookId::of("alloc_contiguous");
pub const PHYSMEM_FREE: HookId = HookId::of("free");
pub const PHYSMEM_RESERVE: HookId = HookId::of("reserve");
pub const PHYSMEM_STATS: HookId = HookId::of("stats");

pub const PHYSMEM_DOC: &str = "if you call alloc_frames(n), you get a \
MemRegion capability covering n physical frames; alloc_contiguous(n) is the \
same for a contiguous run; reserve(start, len) reserves a physical range; \
stats() reports the allocator's reach; free(region) hands the region's CapId \
and its frames are returned to the allocator.";

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

static PHYSMEM_CONTRACTS: &[ContractId] = &[PHYSMEM_CONTRACT];

/// The frame-pool node. The allocator is a process-lifetime global, reached
/// through `get_phys_allocator_mut()` — the node holds no allocator reference
/// because the bitmap is not a stable point we can capture; `BitmapAllocator`
/// is moved into `Kernel`, and its current address is stashed by the heap.
pub struct PhysMemNode;

impl PhysMemNode {
    /// Stable identity of the frame-pool family root (§7.10.1).
    pub const OBJ_ID: ObjId = ObjId(0x11_0000);
}

impl Obj for PhysMemNode {
    fn obj_id(&self) -> ObjId { Self::OBJ_ID }
    fn kind(&self) -> &'static str { "physmem:node" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&PHYSMEM_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { PHYSMEM_CONTRACTS }

    fn dispatch(
        &self,
        caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == PHYSMEM_ALLOC_FRAMES {
            return self.alloc_frames(arg_u64(args, 0).unwrap_or(1) as usize);
        }
        if hook == PHYSMEM_ALLOC_CONTIG {
            return self.alloc_contiguous(arg_u64(args, 0).unwrap_or(1) as usize);
        }
        if hook == PHYSMEM_FREE {
            let cap = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            return release_region(caller, cap);
        }
        if hook == PHYSMEM_RESERVE {
            let start = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let len = arg_u64(args, 1).ok_or(ObjError::Denied)?;
            heap::get_phys_allocator_mut().reserve_range(start, len);
            return Ok(Reply::None);
        }
        if hook == PHYSMEM_STATS {
            let a = heap::get_phys_allocator_mut();
            return Ok(Reply::Data(vec![
                Value::U64(a.total_frames() as u64),
                Value::U64(a.alloc_end()),
            ]));
        }
        Err(ObjError::NotSupported)
    }
}

impl PhysMemNode {
    /// Hand out `count` physical frames wrapped in a pre-built `MemRegion`
    /// node — no allocation to construct the capability. A `MemRegion` names a
    /// single `base`+`size`, so the `count` frames must be a contiguous run
    /// (the bitmap's run allocator); `alloc_frames(n)` is then `n` contiguous
    /// frames, indistinguishable in shape from `alloc_contiguous(n)`.
    fn alloc_frames(&self, count: usize) -> Result<Reply, ObjError> {
        let pool = mem_region_pool(RegionKind::Phys);
        dma_trace!({
            use crate::drivers::serial::SerialPort;
            SerialPort::puts("[DBG:physmem] alloc_frames(");
            SerialPort::put_u64(count as u64);
            SerialPort::puts(") pool_free=");
            SerialPort::put_u64(pool.len() as u64);
            SerialPort::puts("\n");
        });
        let Some(node) = pool.take() else {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:physmem] POOL EMPTY (Phys wrapper pool exhausted) -> OOM\n");
            });
            return Err(ObjError::OutOfMemory);
        };
        let alloc = heap::get_phys_allocator_mut();
        let Some(base) = alloc.alloc_contiguous(count) else {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:physmem] FRAME ALLOC FAIL alloc_contiguous(");
                SerialPort::put_u64(count as u64);
                SerialPort::puts(") next_free=");
                SerialPort::put_u64(alloc.next_free() as u64);
                SerialPort::puts(" total=");
                SerialPort::put_u64(alloc.total_frames() as u64);
                SerialPort::puts(" -> OOM\n");
            });
            pool.recycle(node);
            return Err(ObjError::OutOfMemory);
        };
        node.set_region(base, (count as u64) * 4096, 4096);
        dma_trace!({
            use crate::drivers::serial::SerialPort;
            SerialPort::puts("[DBG:physmem] alloc_frames OK base=0x");
            SerialPort::put_hex(base);
            SerialPort::puts("\n");
        });
        Ok(Reply::Caps(vec![region_cap(node)]))
    }

    /// Allocate one contiguous run of `count` frames; wrapped as a single
    /// `MemRegion`. OOM recycles the pre-built wrapper and reports OOM.
    fn alloc_contiguous(&self, count: usize) -> Result<Reply, ObjError> {
        let pool = mem_region_pool(RegionKind::Phys);
        dma_trace!({
            use crate::drivers::serial::SerialPort;
            SerialPort::puts("[DBG:physmem] alloc_contiguous(");
            SerialPort::put_u64(count as u64);
            SerialPort::puts(") pool_free=");
            SerialPort::put_u64(pool.len() as u64);
            SerialPort::puts("\n");
        });
        let Some(node) = pool.take() else {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:physmem] POOL EMPTY (Phys wrapper pool exhausted) -> OOM\n");
            });
            return Err(ObjError::OutOfMemory);
        };
        let alloc = heap::get_phys_allocator_mut();
        let Some(base) = alloc.alloc_contiguous(count) else {
            dma_trace!({
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[DBG:physmem] FRAME ALLOC FAIL alloc_contiguous(");
                SerialPort::put_u64(count as u64);
                SerialPort::puts(") next_free=");
                SerialPort::put_u64(alloc.next_free() as u64);
                SerialPort::puts(" total=");
                SerialPort::put_u64(alloc.total_frames() as u64);
                SerialPort::puts(" -> OOM\n");
            });
            pool.recycle(node);
            return Err(ObjError::OutOfMemory);
        };
        node.set_region(base, (count as u64) * 4096, 4096);
        dma_trace!({
            use crate::drivers::serial::SerialPort;
            SerialPort::puts("[DBG:physmem] alloc_contiguous OK base=0x");
            SerialPort::put_hex(base);
            SerialPort::puts("\n");
        });
        Ok(Reply::Caps(vec![region_cap(node)]))
    }
}

// ── Heap — dynamic allocation (§7.10.2) ────────────────────────────────

pub const HEAP_CONTRACT: ContractId =
    ContractId::of("heap:allocation", &HEAP_SURFACE, &HEAP_HOOKS);
pub const HEAP_ALLOC: HookId = HookId::of("alloc");
pub const HEAP_FREE: HookId = HookId::of("free");
pub const HEAP_STATS: HookId = HookId::of("stats");

pub const HEAP_DOC: &str = "if you call alloc(size, align), you get a Heap \
MemRegion capability over a block allocated from the kernel arena; \
free(region) deallocates the block back to the arena; stats() is reserved \
pending a public heap arena accessor.";

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

static HEAP_CONTRACTS: &[ContractId] = &[HEAP_CONTRACT];

/// The heap node. `alloc` delegates to the process-global allocator — the same
/// one the `#[global_allocator]` uses — and never allocates to *build* the
/// returned `MemRegion` wrapper (it comes from the pre-built heap pool).
pub struct HeapNode;

impl HeapNode {
    /// Stable identity of the heap family root (§7.10.2).
    pub const OBJ_ID: ObjId = ObjId(0x11_0001);
}

impl Obj for HeapNode {
    fn obj_id(&self) -> ObjId { Self::OBJ_ID }
    fn kind(&self) -> &'static str { "heap:node" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&HEAP_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { HEAP_CONTRACTS }

    fn dispatch(
        &self,
        caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == HEAP_ALLOC {
            let size = arg_u64(args, 0).unwrap_or(1) as usize;
            let align = arg_u64(args, 1).unwrap_or(8) as usize;
            return self.heap_alloc(size, align);
        }
        if hook == HEAP_FREE {
            let cap = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            return release_region(caller, cap);
        }
        if hook == HEAP_STATS {
            // No public arena-stats accessor on heap.rs yet; reply nothing.
            return Ok(Reply::None);
        }
        Err(ObjError::NotSupported)
    }
}

impl HeapNode {
    fn heap_alloc(&self, size: usize, align: usize) -> Result<Reply, ObjError> {
        let Some(node) = mem_region_pool(RegionKind::Heap).take() else {
            return Err(ObjError::OutOfMemory);
        };
        let layout = Layout::from_size_align(size, align).map_err(|_| ObjError::Denied)?;
        let ptr = unsafe { alloc_bytes(layout) };
        if ptr.is_null() {
            mem_region_pool(RegionKind::Heap).recycle(node);
            return Err(ObjError::OutOfMemory);
        }
        node.set_region(ptr as u64, size as u64, align as u64);
        Ok(Reply::Caps(vec![region_cap(node)]))
    }
}

// — AddressSpace — page tables (§7.10.3) ────────────────────────────────

pub const ADDRSPACE_CONTRACT: ContractId =
    ContractId::of("mm:address_space", &ADDRSPACE_SURFACE, &ADDRSPACE_HOOKS);
pub const ADDRSPACE_MAP: HookId = HookId::of("map");
pub const ADDRSPACE_UNMAP: HookId = HookId::of("unmap");
pub const ADDRSPACE_PROTECT: HookId = HookId::of("protect");
pub const ADDRSPACE_SHOOTDOWN: HookId = HookId::of("shootdown");
pub const ADDRSPACE_TRANSLATE: HookId = HookId::of("translate");
pub const ADDRSPACE_ROOT: HookId = HookId::of("root");

pub const ADDRSPACE_DOC: &str = "if you call map(va, phys, size, flags), the \
range is mapped through the page walk rooted at this node; unmap(va, size) \
unmaps it; shootdown() flushes the local TLB; translate(va) resolves a VA; \
root() reports the page-table root. protect is reserved pending a PTE-walk \
mutation API.";

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

static ADDRSPACE_CONTRACTS: &[ContractId] = &[ADDRSPACE_CONTRACT];

/// The address-space node: wraps the page tables of the (shared) root captured
/// at construction. The page-walk itself is reached via `Vmm::from_root`;
/// intermediate-table frames come from `get_phys_allocator_mut` at map/unmap.
pub struct AddressSpaceNode {
    root: u64,
}

impl AddressSpaceNode {
    /// Stable identity of the address-space family root (§7.10.3).
    pub const OBJ_ID: ObjId = ObjId(0x11_0002);

    pub const fn new(root: u64) -> Self {
        AddressSpaceNode { root }
    }
}

impl Obj for AddressSpaceNode {
    fn obj_id(&self) -> ObjId { Self::OBJ_ID }
    fn kind(&self) -> &'static str { "mm:addrspace" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&ADDRSPACE_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { ADDRSPACE_CONTRACTS }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == ADDRSPACE_MAP {
            let va = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let pa = arg_u64(args, 1).ok_or(ObjError::Denied)?;
            let size = arg_u64(args, 2).ok_or(ObjError::Denied)?;
            let flags = arg_u64(args, 3).unwrap_or(0);
            let mut vmm = Vmm::from_root(self.root);
            vmm.map(heap::get_phys_allocator_mut(), va, pa, size, page_flags(flags));
            return Ok(Reply::None);
        }
        if hook == ADDRSPACE_UNMAP {
            let va = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let size = arg_u64(args, 1).ok_or(ObjError::Denied)?;
            let mut vmm = Vmm::from_root(self.root);
            vmm.unmap(heap::get_phys_allocator_mut(), va, size);
            return Ok(Reply::None);
        }
        if hook == ADDRSPACE_PROTECT {
            // No public PTE-walk mutation API exists yet to retag a mapping;
            // reserved for a future `Vmm::protect`. Refuse honestly.
            let _ = (arg_u64(args, 0), arg_u64(args, 1));
            return Err(ObjError::NotSupported);
        }
        if hook == ADDRSPACE_SHOOTDOWN {
            // Local TLB flush (arch-agnostic); the cross-CPU broadcast routes
            // through the Cpu family per §7.10.3 and is a follow-on refinement.
            crate::mm::vmm::flush_tlb();
            return Ok(Reply::None);
        }
        if hook == ADDRSPACE_TRANSLATE {
            let va = arg_u64(args, 0).ok_or(ObjError::Denied)?;
            let vmm = Vmm::from_root(self.root);
            return match vmm.translate(va) {
                Some(pa) => Ok(Reply::Data(vec![Value::U64(pa)])),
                None => Err(ObjError::Denied),
            };
        }
        if hook == ADDRSPACE_ROOT {
            return Ok(Reply::Data(vec![Value::U64(self.root)]));
        }
        Err(ObjError::NotSupported)
    }
}

// — Cpu family root (§7.10.4) ───────────────────────────────────────────

pub const CPU_CONTRACT: ContractId = ContractId::of("smp:cpu", &CPU_SURFACE, &CPU_HOOKS);
pub const CPU_WAKE: HookId = HookId::of("wake");
pub const CPU_IPI: HookId = HookId::of("ipi");
pub const CPU_SHOOTDOWN: HookId = HookId::of("shootdown");
pub const CPU_STATS: HookId = HookId::of("stats");

pub const CPU_DOC: &str = "the Cpu family: ipi(target, vector) sends an \
interrupt; shootdown() broadcasts a TLB-flush IPI; wake() acknowledges the \
online-set; stats() reports id/count. Per-CPU children are materialized by \
materialize_cpu_child and are the targets of cross-edge shootdown calls.";

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

static CPU_CONTRACTS: &[ContractId] = &[CPU_CONTRACT];

/// The CPU family root. Holds the original `CpuManager` service reference for
/// dispatch; per-CPU children share this contract and are materialized against
/// the same root (parent edge = this root's id).
pub struct CpuRootNode {
    cpu: &'static dyn CpuManager,
}

impl CpuRootNode {
    /// Stable identity of the CPU family root (§7.10.4).
    pub const OBJ_ID: ObjId = ObjId(0x11_0003);

    pub const fn new(cpu: &'static dyn CpuManager) -> Self {
        CpuRootNode { cpu }
    }
}

impl Obj for CpuRootNode {
    fn obj_id(&self) -> ObjId { Self::OBJ_ID }
    fn kind(&self) -> &'static str { "cpu:family" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&CPU_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { CPU_CONTRACTS }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == CPU_WAKE {
            // The ApContext list is built by `smp::init`; a capability hook
            // cannot reconstruct it from scalar args, so re-invoking
            // `wake_aps` here would re-run the trampoline broadcast with no
            // targets. Acknowledge the online set instead; the real AP bring-up
            // is the `materialize_cpu_child` path at SMP start.
            let _ = arg_u64(args, 0);
            return Ok(Reply::Data(vec![Value::U64(self.cpu.cpu_count() as u64)]));
        }
        if hook == CPU_IPI {
            let target = arg_u64(args, 0).ok_or(ObjError::Denied)? as u32;
            let vector = arg_u64(args, 1).ok_or(ObjError::Denied)? as u8;
            self.cpu.send_ipi(target, vector);
            return Ok(Reply::None);
        }
        if hook == CPU_SHOOTDOWN {
            self.cpu.broadcast_ipi_except_self(TLB_SHOOTDOWN_VECTOR);
            return Ok(Reply::None);
        }
        if hook == CPU_STATS {
            return Ok(Reply::Data(vec![
                Value::U64(self.cpu.current_cpu_id() as u64),
                Value::U64(self.cpu.cpu_count() as u64),
                Value::U64(0),
            ]));
        }
        Err(ObjError::NotSupported)
    }
}

/// A per-CPU child node, materialized during SMP bring-up against a family
/// root. Its functions are scoped to `cpu_id`; it holds the same `CpuManager`
/// the root does, plus its hardware (APIC/hart) id for the surface/stats.
pub struct CpuNode {
    cpu: &'static dyn CpuManager,
    hw_id: u32,
    cpu_id: u32,
}

impl Obj for CpuNode {
    fn obj_id(&self) -> ObjId { ObjId(CPU_CHILD_ID_BASE + self.cpu_id as u64) }
    fn kind(&self) -> &'static str { "cpu:node" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&CPU_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { CPU_CONTRACTS }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == CPU_WAKE {
            return Ok(Reply::Data(vec![Value::U64(self.cpu.cpu_count() as u64)]));
        }
        if hook == CPU_IPI {
            let target = arg_u64(args, 0).unwrap_or(self.cpu_id as u64) as u32;
            let vector = arg_u64(args, 1).ok_or(ObjError::Denied)? as u8;
            self.cpu.send_ipi(target, vector);
            return Ok(Reply::None);
        }
        if hook == CPU_SHOOTDOWN {
            self.cpu.send_ipi(self.cpu_id, TLB_SHOOTDOWN_VECTOR);
            return Ok(Reply::None);
        }
        if hook == CPU_STATS {
            let (hw_id, started) = match crate::smp::find_cpu_by_hardware_id(self.hw_id) {
                Some((pc, _)) => (pc.apic_id, pc.started.load(Ordering::Relaxed)),
                None => (self.hw_id, 0),
            };
            return Ok(Reply::Data(vec![
                Value::U64(hw_id as u64),
                Value::U64(self.cpu_id as u64),
                Value::U64(started),
            ]));
        }
        Err(ObjError::NotSupported)
    }
}

// — Irq family (§7.10.5) ────────────────────────────────────────────────

pub const IRQ_CONTRACT: ContractId = ContractId::of("irq:vector", &IRQ_SURFACE, &IRQ_HOOKS);
pub const IRQ_REGISTER: HookId = HookId::of("register_handler");
pub const IRQ_UNREGISTER: HookId = HookId::of("unregister");
pub const IRQ_ACK: HookId = HookId::of("ack");
pub const IRQ_SET_ENABLED: HookId = HookId::of("set_enabled");

pub const IRQ_DOC: &str = "if you call register_handler(vector, handler_cap), \
a handler node's fn() (materialized by the kernel, never a raw caller address) \
is bound to the vector; MSI allocates a free device vector when vector is \
omitted; unregister(vector); ack() sends EOI; set_enabled(vector, bool). \
Per-vector children are materialized by materialize_irq_child.";

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

static IRQ_CONTRACTS: &[ContractId] = &[IRQ_CONTRACT];

/// The interrupt family root. Holds the `InterruptManager` and `MsiAllocator`
/// service references; per-vector children share the same contract and are
/// registered against this root.
pub struct IrqRootNode {
    irq: &'static dyn InterruptManager,
    msi: &'static dyn MsiAllocator,
}

impl IrqRootNode {
    /// Stable identity of the interrupt family root (§7.10.5).
    pub const OBJ_ID: ObjId = ObjId(0x11_0004);

    pub const fn new(
        irq: &'static dyn InterruptManager,
        msi: &'static dyn MsiAllocator,
    ) -> Self {
        IrqRootNode { irq, msi }
    }
}

impl Obj for IrqRootNode {
    fn obj_id(&self) -> ObjId { Self::OBJ_ID }
    fn kind(&self) -> &'static str { "irq:family" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&IRQ_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { IRQ_CONTRACTS }

    fn dispatch(
        &self,
        caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == IRQ_REGISTER {
            let handler = resolve_handler(caller, arg_u64(args, 1).ok_or(ObjError::Denied)?)?;
            let vector = match arg_u64(args, 0) {
                Some(v) => v as u8,
                // Omitted vector → ask MSI for a free device vector.
                None => match self.msi.allocate_device_vector(handler) {
                    Some(v) => v,
                    None => return Err(ObjError::Exhausted),
                },
            };
            self.irq.register_handler(vector, handler);
            return Ok(Reply::None);
        }
        if hook == IRQ_UNREGISTER {
            let vector = arg_u64(args, 0).ok_or(ObjError::Denied)? as u8;
            self.irq.unregister_handler(vector);
            self.msi.release_device_vector(vector);
            return Ok(Reply::None);
        }
        if hook == IRQ_ACK {
            self.irq.eoi();
            return Ok(Reply::None);
        }
        if hook == IRQ_SET_ENABLED {
            let vector = arg_u64(args, 0).unwrap_or(0) as u8;
            if arg_u64(args, 1).unwrap_or(0) != 0 {
                self.irq.enable(vector);
            } else {
                self.irq.disable(vector);
            }
            return Ok(Reply::None);
        }
        Err(ObjError::NotSupported)
    }
}

/// A per-vector interrupt child node, scoped to one interrupt line. Bind a
/// handler node's `fn()`, ack, or toggle enable on this vector only.
pub struct IrqNode {
    irq: &'static dyn InterruptManager,
    msi: &'static dyn MsiAllocator,
    vector: u8,
}

impl Obj for IrqNode {
    fn obj_id(&self) -> ObjId { ObjId(IRQ_CHILD_ID_BASE + self.vector as u64) }
    fn kind(&self) -> &'static str { "irq:node" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&IRQ_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { IRQ_CONTRACTS }

    fn dispatch(
        &self,
        caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == IRQ_REGISTER {
            let handler = resolve_handler(caller, arg_u64(args, 1).ok_or(ObjError::Denied)?)?;
            self.irq.register_handler(self.vector, handler);
            return Ok(Reply::None);
        }
        if hook == IRQ_UNREGISTER {
            self.irq.unregister_handler(self.vector);
            self.msi.release_device_vector(self.vector);
            return Ok(Reply::None);
        }
        if hook == IRQ_ACK {
            self.irq.eoi();
            return Ok(Reply::None);
        }
        if hook == IRQ_SET_ENABLED {
            if arg_u64(args, 1).unwrap_or(0) != 0 {
                self.irq.enable(self.vector);
            } else {
                self.irq.disable(self.vector);
            }
            return Ok(Reply::None);
        }
        Err(ObjError::NotSupported)
    }
}

// ── The handler node — a vetted interrupt entry point (§7.10.5) ────────

/// Contract of a handler node: a kernel-materialized wrapper over a `fn()`
/// interrupt entry point. `register_handler` on the `Irq` family binds the
/// handler a caller points at *by capability*, never by a raw address the
/// caller supplies.
pub const IRQ_HANDLER_CONTRACT: ContractId =
    ContractId::of("irq:handler", &IRQ_HANDLER_SURFACE, &IRQ_HANDLER_HOOKS);

/// Hook: reply the handler's entry address (for forensics/registry only; the
/// `Irq` family reads the entry through [`Obj::as_handler`], not this hook).
pub const IRQ_HANDLER_ENTRY: HookId = HookId::of("entry");

const IRQ_HANDLER_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "irq:handler",
    attrs: &[SurfaceAttr { name: "entry", ty: TypeTag::U64 }],
    events: &[],
};

const IRQ_HANDLER_HOOKS: &[HookSignature] = &[HookSignature {
    name: "entry",
    params: &[],
    reply: ReplyTag::Data(&[TypeTag::U64]),
}];

static IRQ_HANDLER_CONTRACTS: &[ContractId] = &[IRQ_HANDLER_CONTRACT];

/// Dynamic id base for handler nodes (`0x11_3000` upward, above the per-vector
/// children at `0x11_2000`).
pub const IRQ_HANDLER_ID_BASE: u64 = 0x11_3000;
static IRQ_HANDLER_SEQ: AtomicU64 = AtomicU64::new(0);

/// A kernel-materialized interrupt entry point. Only these nodes implement
/// [`Obj::as_handler`], so the `Irq` family can bind a handler from a
/// capability with the confidence that the address is a vetted kernel `fn()` —
/// a caller can never inject an arbitrary function address through the hooks.
pub struct IrqHandlerNode {
    id: ObjId,
    entry: fn(),
}

impl Obj for IrqHandlerNode {
    fn obj_id(&self) -> ObjId { self.id }
    fn kind(&self) -> &'static str { "irq:handler" }
    fn surface(&self) -> Option<&'static SurfaceDesc> { Some(&IRQ_HANDLER_SURFACE) }
    fn contracts(&self) -> &'static [ContractId] { IRQ_HANDLER_CONTRACTS }

    fn as_handler(&self) -> Option<fn()> {
        Some(self.entry)
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == IRQ_HANDLER_ENTRY {
            return Ok(Reply::Data(vec![Value::U64(self.entry as usize as u64)]));
        }
        Err(ObjError::NotSupported)
    }
}

/// Materialize a handler node over a vetted kernel `fn()`, register it in the
/// store (parent = none — it is an infrastructure leaf, not a family child),
/// and return it as a capability the caller can pass to `register_handler`.
/// The `fn()` itself is the kernel's own interrupt entry — the node only
/// *names* it; nothing in the arg stream ever carries a raw address.
pub fn handler_node(entry: fn()) -> Arc<dyn Obj> {
    let node: Arc<dyn Obj> = Arc::new(IrqHandlerNode {
        id: ObjId(IRQ_HANDLER_ID_BASE + IRQ_HANDLER_SEQ.fetch_add(1, Ordering::Relaxed)),
        entry,
    });
    object_store().register_with_id(node.obj_id(), node.kind(), None);
    node
}

/// Resolve a handler from a capability id in the caller's table. The node must
/// implement [`Obj::as_handler`] (only kernel-materialized handler nodes do);
/// the returned `fn()` is the node's own vetted entry — never a caller-supplied
/// scalar address.
fn resolve_handler(caller: &CapabilityTable, cap_id: u64) -> Result<fn(), ObjError> {
    let node = caller.get(CapId(cap_id)).map_err(|_| ObjError::NoSuchCap)?;
    node.as_handler().ok_or(ObjError::Denied)
}

// — Child materializers ─────────────────────────────────────────────────
// The family children are materialized by SMP bring-up / IRQ registration. The
// fixed materializer signatures do not take a service container, so the
// `&'static dyn` service references seeded by `build_physical_nodes` are
// captured in module `Once`s for the children to share. This is the P3
// materializer seam: only these two helpers read the statics.

static CPU_SERVICE: Once<&'static dyn CpuManager> = Once::new();
static IRQ_SERVICE: Once<&'static dyn InterruptManager> = Once::new();
static MSI_SERVICE: Once<&'static dyn MsiAllocator> = Once::new();
/// The page-table root captured at construction, for the endowing domains to
/// build their own (equal) `AddressSpaceNode` (§6.2).
static ADDR_SPACE_ROOT: Once<u64> = Once::new();

/// Materialize a per-CPU child node under the family root `root_id` for the
/// given hardware (APIC/hart) id. Registers the child (parent = `root_id`) in
/// the store; its `ObjId` is `0x11_1000 + cpu_id`.
pub fn materialize_cpu_child(root_id: ObjId, hw_id: u32) -> Arc<dyn Obj> {
    let cpu = *CPU_SERVICE
        .get()
        .expect("materialize_cpu_child: build_physical_nodes must run first");
    let cpu_id = crate::smp::find_cpu_by_hardware_id(hw_id)
        .map(|(_, id)| id)
        .unwrap_or(hw_id);
    let node: Arc<dyn Obj> = Arc::new(CpuNode { cpu, hw_id, cpu_id });
    object_store().register_with_id(node.obj_id(), node.kind(), Some(root_id));
    node
}

/// Materialize a per-vector interrupt node under the family root `root_id`.
/// Its `ObjId` is `0x11_2000 + vector`.
pub fn materialize_irq_child(root_id: ObjId, vector: u8) -> Arc<dyn Obj> {
    let irq = *IRQ_SERVICE
        .get()
        .expect("materialize_irq_child: build_physical_nodes must run first");
    let msi = *MSI_SERVICE
        .get()
        .expect("materialize_irq_child: build_physical_nodes must run first");
    let node: Arc<dyn Obj> = Arc::new(IrqNode { irq, msi, vector });
    object_store().register_with_id(node.obj_id(), node.kind(), Some(root_id));
    node
}

// — Bootstrap accessor ──────────────────────────────────────────────────

/// The constructed physical-world nodes, for endowing the boot domain (§5.4).
pub struct PhysicalNodes {
    pub physmem: Arc<dyn Obj>,
    pub heap: Arc<dyn Obj>,
    pub addrspace: Arc<dyn Obj>,
    pub cpu_root: Arc<dyn Obj>,
    pub irq_root: Arc<dyn Obj>,
}

/// Construct the five physical nodes, register their stable ids + kinds in the
/// ObjectStore (roots with `parent = None`), seed the child-materializer
/// service references, and return them so the bootstrap agent can endow the
/// boot domain. The nodes hold the original service/allocator references for
/// dispatch.
pub fn build_physical_nodes(
    page_table_root: u64,
    svc: &'static KernelServices,
) -> PhysicalNodes {
    CPU_SERVICE.call_once(|| svc.cpu);
    IRQ_SERVICE.call_once(|| svc.interrupts);
    MSI_SERVICE.call_once(|| svc.msi);
    ADDR_SPACE_ROOT.call_once(|| page_table_root);

    let physmem: Arc<dyn Obj> = Arc::new(PhysMemNode);
    let heap: Arc<dyn Obj> = Arc::new(HeapNode);
    let addrspace: Arc<dyn Obj> = Arc::new(AddressSpaceNode::new(page_table_root));
    let cpu_root: Arc<dyn Obj> = Arc::new(CpuRootNode::new(svc.cpu));
    let irq_root: Arc<dyn Obj> = Arc::new(IrqRootNode::new(svc.interrupts, svc.msi));

    let store = object_store();
    store.register_with_id(physmem.obj_id(), physmem.kind(), None);
    store.register_with_id(heap.obj_id(), heap.kind(), None);
    store.register_with_id(addrspace.obj_id(), addrspace.kind(), None);
    store.register_with_id(cpu_root.obj_id(), cpu_root.kind(), None);
    store.register_with_id(irq_root.obj_id(), irq_root.kind(), None);

    PhysicalNodes { physmem, heap, addrspace, cpu_root, irq_root }
}

// ── Per-node accessors for the endowing domains (§6.2) ─────────────────
//
// The first driver domain is endowed with the P3 physical nodes it needs but
// is created after `build_physical_nodes` has returned its `PhysicalNodes`.
// Rather than thread the whole struct through `driver::create()`, expose the
// two nodes the driver domain holds over the shared providers. Both nodes are
// value-constructible: `PhysMemNode` is a unit struct (same identity as the
// built one), and `AddressSpaceNode` carries only the captured `root` — so an
// independently built node is *equal* to the singleton the boot domain holds
// (§7.10.3). Callable after `build_physical_nodes` (starts the driver).

/// The frame-pool family root (`PhysMemNode`), equal to the one `build_physical_nodes`
/// returned (§6.2).
pub fn phys_mem_node() -> Arc<dyn Obj> {
    Arc::new(PhysMemNode)
}

/// The address-space family root, built over the page-table root captured by
/// `build_physical_nodes` (equal to its `AddressSpaceNode`).
pub fn addr_space_node() -> Arc<dyn Obj> {
    let root = *ADDR_SPACE_ROOT.get().expect("build_physical_nodes must run first");
    Arc::new(AddressSpaceNode::new(root))
}

// ── Small helpers (mirror `adapters.rs`) ───────────────────────────────

/// Read a `Value::U64` hook argument, or `None`.
fn arg_u64(args: &Args, i: usize) -> Option<u64> {
    match args.vals.get(i) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

/// Provider-side `free(region)`: take the region's `CapId` from the caller's
/// table, verify it names a `mem:region` node, and delegate to the region's own
/// `free` hook — which returns the backing to its allocator and recycles the
/// pooled wrapper (§ `memregion.rs`). The `get` is PERMIT-less by design (§7.4
/// item 3: our own dispatch already passed PERMIT), but the contract-membership
/// check keeps a caller from pointing `free` at a foreign node.
fn release_region(caller: &CapabilityTable, cap_id: u64) -> Result<Reply, ObjError> {
    let id = CapId(cap_id);
    let node = caller.get(id).map_err(|_| ObjError::NoSuchCap)?;
    if !node.contracts().contains(&MEM_REGION_CONTRACT) {
        return Err(ObjError::Denied);
    }
    invoke(caller, id, MEM_REGION_CONTRACT, MEM_REGION_FREE, &Args::none())
}

/// Build the capability that names a handed-out `MemRegion`. The node wraps a
/// page of memory, so the cap carries `INVOKE|QUERY` over the `mem:region`
/// contract ([`MEM_REGION_CONTRACT`]) — enough to read/write/recycle it.
fn region_cap(node: Arc<MemRegionNode>) -> CapHandle {
    let node: Arc<dyn Obj> = node;
    CapHandle {
        id: CapId(0),
        node,
        rights: CapRights::new(Rights::INVOKE.or(Rights::QUERY), ContractRights::empty()),
        state: HandleState::Live,
    }
}

/// Rebuild a `PageFlags` from the raw bits passed through a hook, using only
/// the public flag constants. `PageFlags::READ & PageFlags::WRITE` is `0`
/// (their bits do not overlap), giving us a zero start without a public
/// empty-flags constructor.
fn page_flags(flags: u64) -> PageFlags {
    let mut f = PageFlags::READ & PageFlags::WRITE;
    if flags & 1 != 0 { f |= PageFlags::READ; }
    if flags & 2 != 0 { f |= PageFlags::WRITE; }
    if flags & 4 != 0 { f |= PageFlags::EXECUTE; }
    if flags & 8 != 0 { f |= PageFlags::NO_CACHE; }
    if flags & 16 != 0 { f |= PageFlags::USER; }
    if flags & 32 != 0 { f |= PageFlags::WRITE_COMBINING; }
    f
}