//! MM provider — kernel memory-management introspection.
//!
//! Split per your layout:
//! - `/sys/mm/layout`  RO snapshot of virtual layout (no methods)
//! - `/kernel/mm/heap`  RW introspection (chunks list is RO but lives under /kernel)
//! - `/kernel/mm/phys`  phys allocator snapshot
//! - `/kernel/mm/vmm`   VMM global snapshot + methods (`:translate`)
//! - `/kernel/mm/usermem` summary + `/kernel/mm/fault` counter

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::Ordering;

use super::super::schema::{self, Field, MethodDesc, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    // /sys/mm/layout — RO
    let layout_obj = Arc::new(LayoutObject);
    crate::unispace::connect("/sys/mm/layout", layout_obj)?;
    // heap and vmm are Service-with-children hybrids so /heap/chunks and
    // /vmm/clones are reachable while read() still returns the Service value.
    let heap_dir = Arc::new(crate::unispace::dir::ServiceDir::new(Arc::new(HeapObject)));
    crate::unispace::connect("/kernel/mm/heap", heap_dir.clone())?;
    crate::unispace::connect("/kernel/mm/heap/chunks", Arc::new(HeapChunksObject))?;
    crate::unispace::connect("/kernel/mm/phys", Arc::new(PhysObject))?;
    let vmm_dir = Arc::new(crate::unispace::dir::ServiceDir::new(Arc::new(VmmObject)));
    crate::unispace::connect("/kernel/mm/vmm", vmm_dir.clone())?;
    crate::unispace::connect("/kernel/mm/vmm/clones", Arc::new(VmmClonesObject))?;
    crate::unispace::connect("/kernel/mm/vmm/cpu_roots", Arc::new(VmmCpuRootsObject))?;
    // keep flat aliases for tooling that expects the old sibling names
    crate::unispace::connect("/kernel/mm/heap_chunks", Arc::new(HeapChunksObject))?;
    crate::unispace::connect("/kernel/mm/vmm_clones", Arc::new(VmmClonesObject))?;
    crate::unispace::connect("/kernel/mm/vmm_cpu_roots", Arc::new(VmmCpuRootsObject))?;
    crate::unispace::connect("/kernel/mm/fault", Arc::new(FaultObject))?;
    crate::unispace::connect("/sys/mm/physmap", Arc::new(PhysmapObject))?;
    Ok(())
}

// ── /sys/mm/layout ──
static LAYOUT_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "vma_base", ty: &schema::SCHEMA_U64 },
    Field { name: "heap_floor", ty: &schema::SCHEMA_U64 },
    Field { name: "heap_top", ty: &schema::SCHEMA_U64 },
    Field { name: "kstack_base", ty: &schema::SCHEMA_U64 },
    Field { name: "kstack_floor", ty: &schema::SCHEMA_U64 },
    Field { name: "physmap_base", ty: &schema::SCHEMA_U64 },
    Field { name: "physmap_size", ty: &schema::SCHEMA_U64 },
    Field { name: "kaslr_offset", ty: &schema::SCHEMA_U64 },
    Field { name: "kaslr_enabled", ty: &schema::SCHEMA_BOOL },
    Field { name: "fb_base", ty: &schema::SCHEMA_U64 },
    Field { name: "fb_floor", ty: &schema::SCHEMA_U64 },
    Field { name: "lapic_base", ty: &schema::SCHEMA_U64 },
]);

struct LayoutObject;
impl Object for LayoutObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &LAYOUT_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::Struct(vec![
            Value::U64(crate::mm::layout::KERNEL_VMA_BASE),
            Value::U64(crate::mm::layout::HEAP_FLOOR),
            Value::U64(crate::mm::layout::HEAP_TOP),
            Value::U64(crate::mm::layout::KSTACK_VADDR_BASE),
            Value::U64(crate::mm::layout::KSTACK_VADDR_FLOOR),
            Value::U64(crate::mm::layout::PHYS_MAP_BASE),
            Value::U64(crate::mm::layout::physmap_end()),
            Value::U64(crate::mm::layout::kaslr_offset()),
            Value::Bool(crate::mm::layout::kaslr_offset() != 0),
            Value::U64(crate::mm::layout::FB_VADDR_BASE),
            Value::U64(crate::mm::layout::FB_VADDR_FLOOR),
            Value::U64(crate::mm::layout::LAPIC_VADDR_BASE),
        ]);
        schema::encode_value(&v, &LAYOUT_SCHEMA, out)
    }
}

// ── /kernel/mm/heap ──
static HEAP_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "low_vaddr", ty: &schema::SCHEMA_U64 },
    Field { name: "chunk_count", ty: &schema::SCHEMA_U64 },
    Field { name: "max_chunks", ty: &schema::SCHEMA_U64 },
    Field { name: "free_list_len", ty: &schema::SCHEMA_U64 },
    Field { name: "heap_top", ty: &schema::SCHEMA_U64 },
    Field { name: "heap_floor", ty: &schema::SCHEMA_U64 },
]);

struct HeapObject;
impl Object for HeapObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &HEAP_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let (low, cnt, flen) = crate::mm::heap::heap_snapshot().map_err(|_| UnispaceError::InvalidArgument)?;
        let v = Value::Struct(vec![
            Value::U64(low),
            Value::U64(cnt as u64),
            Value::U64(crate::mm::heap::MAX_CHUNKS as u64),
            Value::U64(flen as u64),
            Value::U64(crate::mm::layout::HEAP_TOP),
            Value::U64(crate::mm::layout::HEAP_FLOOR),
        ]);
        schema::encode_value(&v, &HEAP_SCHEMA, out)
    }
}

// /kernel/mm/heap/chunks → list of chunks
static CHUNK_ENTRY: Schema = Schema::Struct(&[
    Field { name: "vaddr", ty: &schema::SCHEMA_U64 },
    Field { name: "phys", ty: &schema::SCHEMA_U64 },
    Field { name: "size", ty: &schema::SCHEMA_U64 },
    Field { name: "live", ty: &schema::SCHEMA_U64 },
    Field { name: "scattered", ty: &schema::SCHEMA_BOOL },
]);
static CHUNKS_SCHEMA: Schema = Schema::List(&CHUNK_ENTRY);

struct HeapChunksObject;
impl Object for HeapChunksObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &CHUNKS_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let cnt = crate::mm::heap::heap_chunk_count();
        let mut items = Vec::with_capacity(cnt);
        for i in 0..cnt {
            if let Some((vaddr, phys, size, live, scattered)) = crate::mm::heap::heap_chunk_snapshot(i) {
                items.push(Value::Struct(vec![
                    Value::U64(vaddr),
                    Value::U64(phys),
                    Value::U64(size),
                    Value::U64(live as u64),
                    Value::Bool(scattered),
                ]));
            }
        }
        schema::encode_value(&Value::List(items), &CHUNKS_SCHEMA, out)
    }
}

// ── /kernel/mm/phys ──
static PHYS_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "total_frames", ty: &schema::SCHEMA_U64 },
    Field { name: "free_frames", ty: &schema::SCHEMA_U64 },
    Field { name: "free_exact", ty: &schema::SCHEMA_U64 },
    Field { name: "next_free", ty: &schema::SCHEMA_U64 },
    Field { name: "alloc_end", ty: &schema::SCHEMA_U64 },
    Field { name: "usable_regions", ty: &schema::SCHEMA_U64 },
]);

struct PhysObject;
impl Object for PhysObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PHYS_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        // phys allocator may be null early boot (before heap init), return zeros.
        let (total, free, free_exact, next_free, alloc_end, usable_cnt) = {
            let ptr = crate::mm::heap::phys_allocator_raw();
            if ptr.is_null() {
                (0, 0, 0, 0, 0, 0)
            } else {
                let alloc = unsafe { &*ptr };
                let total = alloc.total_frames() as u64;
                let free = alloc.free_frames() as u64;
                let free_exact = alloc.free_frames_exact() as u64;
                let next_free = alloc.next_free() as u64;
                let alloc_end = alloc.alloc_end();
                let usable_cnt = alloc.usable_regions().len() as u64;
                (total, free, free_exact, next_free, alloc_end, usable_cnt)
            }
        };
        let v = Value::Struct(vec![
            Value::U64(total),
            Value::U64(free),
            Value::U64(free_exact),
            Value::U64(next_free),
            Value::U64(alloc_end),
            Value::U64(usable_cnt),
        ]);
        schema::encode_value(&v, &PHYS_SCHEMA, out)
    }
}

// ── /kernel/mm/vmm ──
static VMM_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "clone_roots", ty: &schema::SCHEMA_U64 },
    Field { name: "tlb_seq", ty: &schema::SCHEMA_U64 },
    Field { name: "half_boundary", ty: &schema::SCHEMA_U64 },
    Field { name: "phys_offset", ty: &schema::SCHEMA_U64 },
]);

struct VmmObject;
impl Object for VmmObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &VMM_SCHEMA }
    fn methods(&self) -> &'static [super::super::schema::MethodDesc] {
        &VMM_METHODS
    }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let (clones, seq, half) = crate::mm::vmm::vmm_global_snapshot();
        let phys_off = crate::mm::layout::phys_offset();
        let v = Value::Struct(vec![
            Value::U64(clones as u64),
            Value::U64(seq),
            Value::U64(half),
            Value::U64(phys_off),
        ]);
        schema::encode_value(&v, &VMM_SCHEMA, out)
    }
    fn invoke(&self, method: usize, v: Value, out: &mut Vec<u8>) -> Result<(), UnispaceError> {
        match method {
            0 => {
                let va = match v {
                    Value::Struct(ref fields) => match fields.get(0) {
                        Some(Value::U64(n)) => *n,
                        _ => return Err(UnispaceError::SchemaMismatch),
                    },
                    _ => return Err(UnispaceError::SchemaMismatch),
                };
                // Restrict to low-half (user) VA to avoid KASLR/phys probing of kernel high half.
                const HALF_BOUNDARY: u64 = 0x0000_8000_0000_0000;
                if va >= HALF_BOUNDARY {
                    return Err(UnispaceError::InvalidArgument);
                }
                #[cfg(target_arch = "x86_64")]
                let root = if let Some(pc) = crate::smp::try_current_per_cpu() {
                    let ptr = pc.current_task.load(Ordering::Relaxed);
                    if !ptr.is_null() {
                        let t = unsafe { &*(ptr as *const crate::task::Task) };
                        if t.root != 0 { t.root } else { crate::task::kernel_root() }
                    } else {
                        crate::task::kernel_root()
                    }
                } else {
                    crate::task::kernel_root()
                };
                #[cfg(not(target_arch = "x86_64"))]
                let root: u64 = {
                    // riscv: no per-task roots; fall back to current satp or report unsupported
                    let satp: u64;
                    unsafe { core::arch::asm!("csrr {}, satp", out(reg) satp) };
                    let ppn = satp & 0xFFF_FFFF_FFFFu64;
                    if ppn == 0 { return Err(UnispaceError::Unsupported); }
                    ppn << 12
                };
                if root == 0 {
                    return Err(UnispaceError::NotFound);
                }
                let vmm = crate::mm::vmm::Vmm::from_root(root);
                let phys = vmm.translate(va).unwrap_or(0);
                let resp = Value::Struct(vec![Value::U64(phys)]);
                schema::encode_value(&resp, &TRANSLATE_OUTPUT, out)
            }
            _ => Err(UnispaceError::MethodNotFound),
        }
    }
}

static TRANSLATE_INPUT: Schema = Schema::Struct(&[Field { name: "vaddr", ty: &schema::SCHEMA_U64 }]);
static TRANSLATE_OUTPUT: Schema = Schema::Struct(&[Field { name: "paddr", ty: &schema::SCHEMA_U64 }]);
static VMM_METHODS: [MethodDesc; 1] = [MethodDesc { name: "translate", input: &TRANSLATE_INPUT, output: &TRANSLATE_OUTPUT }];

// ── /kernel/mm/vmm/clones ──
static CLONES_SCHEMA: Schema = Schema::List(&schema::SCHEMA_U64);
struct VmmClonesObject;
impl Object for VmmClonesObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &CLONES_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let clones = crate::mm::vmm::vmm_clone_roots_snapshot();
        let items = clones.into_iter().map(|r| Value::U64(r)).collect();
        schema::encode_value(&Value::List(items), &CLONES_SCHEMA, out)
    }
}

// ── /kernel/mm/vmm/cpu_roots ──
static CPU_ROOTS_SCHEMA: Schema = Schema::List(&schema::SCHEMA_U64);
struct VmmCpuRootsObject;
impl Object for VmmCpuRootsObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &CPU_ROOTS_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let arr = crate::mm::vmm::vmm_cpu_roots_snapshot();
        let items = arr.iter().map(|&r| Value::U64(r)).collect();
        schema::encode_value(&Value::List(items), &CPU_ROOTS_SCHEMA, out)
    }
}

// ── /kernel/mm/fault ── (#PF count)
struct FaultObject;
impl Object for FaultObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U64 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        #[cfg(target_arch = "x86_64")]
        let n = crate::arch::x86_64::idt::pf_count();
        #[cfg(target_arch = "riscv64")]
        let n = crate::arch::riscv64::trap::pf_count();
        #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
        let n = 0u64;
        schema::encode_value(&Value::U64(n), &schema::SCHEMA_U64, out)
    }
}

// ── /sys/mm/physmap ──
static PHYSMAP_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "base", ty: &schema::SCHEMA_U64 },
    Field { name: "size", ty: &schema::SCHEMA_U64 },
    Field { name: "offset", ty: &schema::SCHEMA_U64 },
]);
struct PhysmapObject;
impl Object for PhysmapObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PHYSMAP_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let v = Value::Struct(vec![
            Value::U64(crate::mm::layout::PHYS_MAP_BASE),
            Value::U64(crate::mm::layout::physmap_end()),
            Value::U64(crate::mm::layout::phys_offset()),
        ]);
        schema::encode_value(&v, &PHYSMAP_SCHEMA, out)
    }
}
