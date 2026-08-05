//! C5 — `MemRegion`: a page of memory is *a thing with a capability*.
//!
//! Wraps a handed-out frame (`RegionKind::Phys`, from the `PhysMem` node over
//! `mm/phys_alloc.rs`) or a heap block (`RegionKind::Heap`, from the `Heap`
//! node over `mm/heap.rs`) as a first-class node. Each `MemRegionNode` is an
//! atom-pool family member (RootGraph §7.10.1): freeing is *revoking the last
//! capability*, and the node itself is the recyclable wrapper that carries a
//! stable `obj_id` plus the region's `base`/`size`.
//!
//! ## The no-alloc-in-alloc-hooks rule (PhysicalNodes phase)
//!
//! The `PhysMem::alloc_frames` / `Heap::alloc` hooks must not allocate to
//! *construct* the returned `MemRegion` capability — frames require frames,
//! heap requires heap. So every wrapper is **pre-built during bootstrap** into
//! a process-global [`MemRegionPool`], one per region kind. The pool's
//! `take()`/`recycle()` only `pop`/`push` `Arc`s — never allocate. A hook that
//! hands out a region merely moves a pre-built node out of its pool.
//!
//! # The `free` hook — ownership and division of labour
//!
//! The *region node itself* owns the release: it knows its kind, `base`, and
//! `size`, so `free` here both returns the backing (physical frames via the
//! bitmap's per-frame `free`, heap blocks via `dealloc` with the stored layout)
//! **and** reclaims the pooled `Arc`. Freeing is thus revoking the capability —
//! the caller drops / invokes `free` on the region cap it holds, and the
//! wrapper is recycled for reuse without any allocation. A second `free` on the
//! same (stale) cap is a no-op: the release zeroes `base`/`size` first.
//!
//! The provider `PhysMem`/`Heap` `free(region)` hooks are thin gateways: they
//! take the region's `CapId` from the caller's table, verify it names a
//! `mem:region` node, and delegate to this hook. The node holds a private
//! self-reference (`slot`) so it can push itself back without requiring an
//! external strong `Arc` at dispatch time.
//!
//! `mem:region` ids are **dynamic**, assigned from [`REGION_ID_BASE`]
//! (`0x12_0000`) upward per materialized node (§7.3: the store stays weak,
//! records hold no node reference).

use alloc::alloc::{dealloc, Layout};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use spin::Once;

use super::contract::{Contract, ContractId, HookSignature, ReplyTag};
use super::hook::HookId;
use super::rights::{CapRights, ContractRights};
use super::store::object_store;
use super::surface::{SurfaceAttr, SurfaceDesc, TypeTag};
use super::table::CapabilityTable;
use super::{Args, Obj, ObjError, ObjId, Reply, Value};

/// The backing of a region: a physical frame or a heap block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionKind {
    Phys,
    Heap,
}

/// Base of the dynamic `mem:region` id space (above the infra/`0x10_xxxx`
/// range so region ids never collide with adapter singletons).
pub const REGION_ID_BASE: u64 = 0x12_0000;

/// Per-kind id stride: each `RegionKind` owns a dedicated `0x1000`-wide block
/// starting at [`REGION_ID_BASE`] + `kind_index * REGION_POOL_STRIDE`, so a
/// pool can be topped up with [`replenish`] without ever colliding with the
/// other kind's wrappers (Phys occupies `0x12_0000..0x12_0fff`, Heap
/// `0x12_1000..0x12_1fff`).
pub const REGION_POOL_STRIDE: u64 = 0x1000;

// ── The contract (content-addressed; mirrors `adapters.rs`) ───────────

pub const MEM_REGION_CONTRACT: ContractId =
    ContractId::of("mem:region", &MEM_REGION_SURFACE, &MEM_REGION_HOOKS);

/// Canonical definition of the `mem:region` contract (§7.8), for the registry.
pub static MEM_REGION_CONTRACT_DEF: Contract = Contract {
    id: MEM_REGION_CONTRACT,
    name: "mem:region",
    surface: &MEM_REGION_SURFACE,
    hooks: &MEM_REGION_HOOKS,
    doc: "a capability over one physical frame run or heap block; base/size read it, free/detach recycle it.",
};

/// Hook: reply `Value::U64(base)` — the region's start address.
pub const MEM_REGION_BASE: HookId = HookId::of("base");

/// Hook: reply `Value::U64(size)` — the region's length.
pub const MEM_REGION_SIZE: HookId = HookId::of("size");

/// Hook: recycle self to its pool, reply `Reply::None`.
pub const MEM_REGION_FREE: HookId = HookId::of("free");

/// Hook: recycle self to its pool *without* returning the backing to its
/// allocator, reply `Reply::None`. Used by the DMA adapter: it hands the
/// caller raw `(phys, virt, size)` scalars, so the driver owns the frames and
/// the pooled wrapper must go back for reuse without freeing them.
pub const MEM_REGION_DETACH: HookId = HookId::of("detach");

const MEM_REGION_SURFACE: SurfaceDesc = SurfaceDesc {
    kind: "mem:region",
    attrs: &[SurfaceAttr { name: "base", ty: TypeTag::U64 }],
    events: &[],
};

const MEM_REGION_HOOKS: &[HookSignature] = &[
    HookSignature {
        name: "base",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "size",
        params: &[],
        reply: ReplyTag::Data(&[TypeTag::U64]),
    },
    HookSignature {
        name: "free",
        params: &[],
        reply: ReplyTag::None,
    },
    HookSignature {
        name: "detach",
        params: &[],
        reply: ReplyTag::None,
    },
];

static MEM_REGION_CONTRACTS: &[ContractId] = &[MEM_REGION_CONTRACT];

// ── The node ──────────────────────────────────────────────────────────

/// A pooled wrapper over a page of memory. `obj_id()` is dynamic and stable:
/// assigned once at materialization, reused for the node's whole pooled life.
pub struct MemRegionNode {
    id: ObjId,
    kind: RegionKind,
    base: AtomicU64,
    size: AtomicU64,
    align: AtomicU64,
    slot: Mutex<Option<Arc<MemRegionNode>>>,
}

impl MemRegionNode {
    /// (Re)assign the physical range this region covers, along with the
    /// alignment that backed it (frames are 4096-aligned; heap blocks carry
    /// the layout's align so `dealloc` can reconstruct the exact layout).
    pub fn set_region(&self, base: u64, size: u64, align: u64) {
        self.base.store(base, Ordering::Relaxed);
        self.size.store(size, Ordering::Relaxed);
        self.align.store(align.max(1), Ordering::Relaxed);
    }

    /// The backing kind of the region's range.
    pub fn region_kind(&self) -> RegionKind {
        self.kind
    }

    /// Return the backing to its allocator: physical frames via the bitmap's
    /// per-frame `free`, heap blocks via `dealloc` with the stored layout.
    /// Called exactly once by the `free` hook, before the fields are zeroed so
    /// a double `free` on a stale cap is a no-op.
    fn release_backing(&self) {
        let base = self.base.load(Ordering::Relaxed);
        if base == 0 {
            return;
        }
        let size = self.size.load(Ordering::Relaxed);
        match self.kind {
            RegionKind::Phys => {
                let alloc = crate::mm::heap::get_phys_allocator_mut();
                let frames = (size + 4095) / 4096;
                for i in 0..frames {
                    // The frames were handed out by this allocator, so per-frame
                    // release is valid; the bitmap bounds-checks the index.
                    unsafe { alloc.free(base + i * 4096) };
                }
            }
            RegionKind::Heap => {
                let align = self.align.load(Ordering::Relaxed) as usize;
                if size > 0 {
                    if let Ok(layout) = Layout::from_size_align(size as usize, align) {
                        unsafe { dealloc(base as *mut u8, layout) };
                    }
                }
            }
        }
    }
}

impl Obj for MemRegionNode {
    fn obj_id(&self) -> ObjId {
        self.id
    }

    fn kind(&self) -> &'static str {
        match self.kind {
            RegionKind::Phys => "phys:mem",
            RegionKind::Heap => "heap:mem",
        }
    }

    fn surface(&self) -> Option<&'static SurfaceDesc> {
        Some(&MEM_REGION_SURFACE)
    }

    fn contracts(&self) -> &'static [ContractId] {
        MEM_REGION_CONTRACTS
    }

    fn surface_value(&self, name: &str) -> Option<Value> {
        match name {
            "base" => Some(Value::U64(self.base.load(Ordering::Relaxed))),
            "size" => Some(Value::U64(self.size.load(Ordering::Relaxed))),
            _ => None,
        }
    }

    fn hook_contract_right(&self, _contract: ContractId, hook: HookId) -> ContractRights {
        match hook {
            MEM_REGION_BASE | MEM_REGION_SIZE => ContractRights::READ,
            MEM_REGION_FREE | MEM_REGION_DETACH => ContractRights::WRITE,
            _ => ContractRights::CALL,
        }
    }

    fn dispatch(
        &self,
        _caller: &CapabilityTable,
        _rights: &CapRights,
        hook: HookId,
        _args: &Args,
    ) -> Result<Reply, ObjError> {
        if hook == MEM_REGION_BASE {
            return Ok(Reply::Data(vec![Value::U64(self.base.load(Ordering::Relaxed))]));
        }
        if hook == MEM_REGION_SIZE {
            return Ok(Reply::Data(vec![Value::U64(self.size.load(Ordering::Relaxed))]));
        }
        if hook == MEM_REGION_FREE {
            // Return the backing to its allocator, zero the identity so a
            // second `free` on a stale cap is a no-op, then reclaim the pooled
            // Arc so the wrapper can be handed out again without allocating.
            self.release_backing();
            self.base.store(0, Ordering::Relaxed);
            self.size.store(0, Ordering::Relaxed);
            let pooled = self.slot.lock().clone();
            return match pooled {
                Some(arc) => {
                    mem_region_pool(self.kind).recycle(arc);
                    Ok(Reply::None)
                }
                None => Err(ObjError::Denied),
            };
        }
        if hook == MEM_REGION_DETACH {
            // Recycle the wrapper without returning the backing. The caller
            // (the DMA adapter) already extracted the raw `phys`/`va` scalars
            // and owns the frames; zeroing `base` first makes a stale cap
            // inert and guards against a double push of the same pooled Arc.
            if self.base.load(Ordering::Relaxed) == 0 {
                return Err(ObjError::Denied);
            }
            self.base.store(0, Ordering::Relaxed);
            self.size.store(0, Ordering::Relaxed);
            let pooled = self.slot.lock().clone();
            return match pooled {
                Some(arc) => {
                    mem_region_pool(self.kind).recycle(arc);
                    Ok(Reply::None)
                }
                None => Err(ObjError::Denied),
            };
        }
        Err(ObjError::NotSupported)
    }
}

// ── The pre-allocated pool (PhysicalNodes phase) ─────────────────────────

/// A process-global set of pre-built region wrappers of one kind.
///
/// `take()` and `recycle()` only `pop`/`push` `Arc`s — no allocation — so a
/// memory-hook may hand out a region without the circular "frames need frames"
/// dependency (PhysicalNodes phase).
pub struct MemRegionPool {
    free: Mutex<Vec<Arc<MemRegionNode>>>,
    kind: RegionKind,
    seq: AtomicU64,
}

impl MemRegionPool {
    /// A fresh (empty) pool of wrappers for `kind`.
    pub const fn new(kind: RegionKind) -> Self {
        MemRegionPool {
            free: Mutex::new(Vec::new()),
            kind,
            seq: AtomicU64::new(0),
        }
    }

    /// Pop a pre-built wrapper (no allocation).
    pub fn take(&self) -> Option<Arc<MemRegionNode>> {
        self.free.lock().pop()
    }

    /// Number of wrappers currently available for hand-out (read-only; used by
    /// the `dma_trace` diagnostics to report pool depth).
    pub fn len(&self) -> usize {
        self.free.lock().len()
    }

    /// Push a wrapper back for reuse (no allocation).
    pub fn recycle(&self, n: Arc<MemRegionNode>) {
        self.free.lock().push(n);
    }

    /// The kind of region this pool serves.
    pub fn kind(&self) -> RegionKind {
        self.kind
    }
}

static PHYS_MEM_POOL: Once<MemRegionPool> = Once::new();
static HEAP_MEM_POOL: Once<MemRegionPool> = Once::new();

/// Access the process-global pre-built pool for a region kind.
pub fn mem_region_pool(kind: RegionKind) -> &'static MemRegionPool {
    match kind {
        RegionKind::Phys => PHYS_MEM_POOL.call_once(|| MemRegionPool::new(RegionKind::Phys)),
        RegionKind::Heap => HEAP_MEM_POOL.call_once(|| MemRegionPool::new(RegionKind::Heap)),
    }
}

// ── Bootstrap: build the pools, register each node, seed the store ────

fn build_pool(kind: RegionKind, capacity: usize) {
    let pool = mem_region_pool(kind);
    for _ in 0..capacity {
        let node = Arc::new(MemRegionNode {
            id: ObjId(pool.seq.fetch_add(1, Ordering::Relaxed)),
            kind,
            base: AtomicU64::new(0),
            size: AtomicU64::new(0),
            align: AtomicU64::new(0),
            slot: Mutex::new(None),
        });
        // Self-reference so `free` can recycle this wrapper at dispatch time.
        *node.slot.lock() = Some(Arc::clone(&node));
        object_store().register_with_id(node.obj_id(), node.kind(), None);
        pool.recycle(Arc::clone(&node));
    }
}

/// Build a pool of `capacity` pre-allocated region wrappers for each kind,
/// assign each a unique `mem:region` `ObjId` (from [`REGION_ID_BASE`] upward,
/// each kind in its own [`REGION_POOL_STRIDE`]-wide block), register them
/// weakly in the store, and seed both pools so `take()` can serve allocator
/// hooks with zero allocation.
///
/// Called once at bootstrap once the heap is up. Phys occupies the lower
/// id block, Heap the next stride block above it.
pub fn materialize_region_pools(capacity: usize) {
    mem_region_pool(RegionKind::Phys)
        .seq
        .store(REGION_ID_BASE, Ordering::Relaxed);
    mem_region_pool(RegionKind::Heap)
        .seq
        .store(REGION_ID_BASE + REGION_POOL_STRIDE, Ordering::Relaxed);
    build_pool(RegionKind::Phys, capacity);
    build_pool(RegionKind::Heap, capacity);
}

/// Top up a pool with `n` fresh pre-built wrappers at its current id sequence
/// (ids stay unique because `seq` keeps advancing). Call only from safe points
/// — never from inside a memory hook — so `take()` stays allocation-free.
pub fn replenish(kind: RegionKind, n: usize) {
    build_pool(kind, n);
}