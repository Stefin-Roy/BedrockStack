use spin::{Mutex, Once};

use crate::mm::layout::region_next_down;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

pub struct DmaBuffer {
    pub phys: u64,
    pub virt: u64,
    pub size: usize,
}

pub trait DmaAllocator: Send + Sync {
    fn alloc_page(&self) -> Option<DmaBuffer>;
    fn alloc_contiguous(&self, count: usize) -> Option<DmaBuffer>;
    fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str>;
    fn virt_to_phys(&self, vaddr: u64) -> Option<u64>;
}

// ── Global DMA allocator singleton ──────────────────────────────────
//
// VA cursor lives in the `dma` window of mm::layout; the two other windows
// (acpi, ecam) never collide because region_next_down is the single source.
static DMA_ALLOCATOR: Once<KernelDma> = Once::new();

pub fn init_dma_allocator(root: u64, alloc: *mut BitmapAllocator) -> &'static dyn DmaAllocator {
    DMA_ALLOCATOR.call_once(|| KernelDma::new(root, alloc))
}

/// C5: return the concrete DMA node for obj-endowment. Call only after
/// `init_dma_allocator` has run (i.e. once the service container exists).
pub fn dma_allocator_static() -> &'static KernelDma {
    DMA_ALLOCATOR.get().expect("dma allocator not initialised")
}

/// Translation cache shared across the kernel.
const TRANS_CACHE_SIZE: usize = 64;
struct TransCacheInner {
    entries: [(u64, u64); TRANS_CACHE_SIZE],
    next: usize,
}
impl TransCacheInner {
    const fn new() -> Self {
        TransCacheInner {
            entries: [(0, 0); TRANS_CACHE_SIZE],
            next: 0,
        }
    }
    fn lookup_or_translate(&mut self, vaddr: u64, root: u64) -> Option<u64> {
        let vaddr_page = vaddr & !0xFFF;
        for &(v, p) in &self.entries {
            if v == vaddr_page {
                return Some(p);
            }
        }
        let pa = Vmm::from_root(root).translate(vaddr_page)?;
        let idx = self.next % TRANS_CACHE_SIZE;
        self.entries[idx] = (vaddr_page, pa);
        self.next = self.next.wrapping_add(1);
        Some(pa)
    }
}
static TRANS_CACHE: Mutex<TransCacheInner> = Mutex::new(TransCacheInner::new());

// ── KernelDma provider ──────────────────────────────────────────────

struct DmaInner {
    alloc: *mut BitmapAllocator,
}

pub struct KernelDma {
    root: u64,
    inner: Mutex<DmaInner>,
}

unsafe impl Send for KernelDma {}
unsafe impl Sync for KernelDma {}

impl KernelDma {
    fn new(root: u64, alloc: *mut BitmapAllocator) -> Self {
        KernelDma {
            root,
            inner: Mutex::new(DmaInner { alloc }),
        }
    }
}

impl DmaAllocator for KernelDma {
    fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        let page_aligned = (size + 4095) & !4095;
        let va = region_next_down("dma", page_aligned).ok_or("DMA: address space exhausted")?;
        let mut inner = self.inner.lock();
        let alloc = unsafe { &mut *inner.alloc };
        Vmm::from_root(self.root).map(
            alloc,
            va,
            paddr,
            page_aligned,
            PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
        );
        Ok(va)
    }

    fn virt_to_phys(&self, vaddr: u64) -> Option<u64> {
        TRANS_CACHE.lock().lookup_or_translate(vaddr, self.root)
    }

    fn alloc_page(&self) -> Option<DmaBuffer> {
        let va = region_next_down("dma", 4096)?;
        let mut inner = self.inner.lock();
        let alloc = unsafe { &mut *inner.alloc };
        let phys = alloc.alloc()?;
        Vmm::from_root(self.root).map(
            alloc,
            va,
            phys,
            4096,
            PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
        );
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) }
        Some(DmaBuffer { phys, virt: va, size: 4096 })
    }

    fn alloc_contiguous(&self, count: usize) -> Option<DmaBuffer> {
        let size = (count as u64) * 4096;
        let va = region_next_down("dma", size)?;
        let mut inner = self.inner.lock();
        let alloc = unsafe { &mut *inner.alloc };
        let phys = alloc.alloc_contiguous(count)?;
        Vmm::from_root(self.root).map(
            alloc,
            va,
            phys,
            size,
            PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
        );
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, size as usize) }
        Some(DmaBuffer { phys, virt: va, size: size as usize })
    }
}
