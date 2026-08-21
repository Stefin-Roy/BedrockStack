use spin::{Mutex, Once};

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{KERNEL_VMA_BASE, PageFlags, Vmm};

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
// Region lives below the PCI ECAM window (KERNEL_VMA_BASE - 0x3000_0000)
// so the two never collide; this is the former blockdriver/AHCI carve-out.
const DMA_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x5000_0000;
const DMA_VADDR_FLOOR: u64 = DMA_VADDR_BASE - 0x2000_0000;

static DMA_ALLOCATOR: Once<KernelDma> = Once::new();

pub fn init_dma_allocator(root: u64, alloc: *mut BitmapAllocator) -> &'static dyn DmaAllocator {
    DMA_ALLOCATOR.call_once(|| KernelDma::new(root, alloc))
}

/// Update the stashed allocator pointer after the `BitmapAllocator` moves
/// (mirrors `heap::set_phys_allocator` / `acpi::update_alloc`).
pub fn update_dma_alloc(alloc: *mut BitmapAllocator) {
    if let Some(kdma) = DMA_ALLOCATOR.get() {
        kdma.inner.lock().alloc = alloc;
    }
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
    fn invalidate_range(&mut self, vaddr: u64, size: u64) {
        if size == 0 {
            return;
        }
        let start = vaddr & !0xFFF;
        let end = (vaddr + size + 0xFFF) & !0xFFF;
        for (v, _) in self.entries.iter_mut() {
            if *v >= start && *v < end {
                *v = 0;
            }
        }
    }
}
static TRANS_CACHE: Mutex<TransCacheInner> = Mutex::new(TransCacheInner::new());

pub fn invalidate_trans_cache(vaddr: u64, size: u64) {
    TRANS_CACHE.lock().invalidate_range(vaddr, size);
}

// ── KernelDma provider ──────────────────────────────────────────────

struct DmaInner {
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
}

pub struct KernelDma {
    root: u64,
    vaddr_floor: u64,
    inner: Mutex<DmaInner>,
}

unsafe impl Send for KernelDma {}
unsafe impl Sync for KernelDma {}

impl KernelDma {
    fn new(root: u64, alloc: *mut BitmapAllocator) -> Self {
        KernelDma {
            root,
            vaddr_floor: DMA_VADDR_FLOOR,
            inner: Mutex::new(DmaInner {
                alloc,
                next_vaddr: DMA_VADDR_BASE,
            }),
        }
    }
}

impl DmaAllocator for KernelDma {
    fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        let page_aligned = (size + 4095) & !4095;
        let mut inner = self.inner.lock();
        let va = inner
            .next_vaddr
            .checked_sub(page_aligned)
            .ok_or("DMA: address space exhausted (overflow)")?;
        if va < self.vaddr_floor {
            return Err("DMA: address space exhausted");
        }
        inner.next_vaddr = va;
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
        let mut inner = self.inner.lock();
        let va = inner.next_vaddr.checked_sub(4096)?;
        if va < self.vaddr_floor {
            return None;
        }
        inner.next_vaddr = va;
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
        Some(DmaBuffer {
            phys,
            virt: va,
            size: 4096,
        })
    }

    fn alloc_contiguous(&self, count: usize) -> Option<DmaBuffer> {
        let size = (count as u64) * 4096;
        let mut inner = self.inner.lock();
        let va = inner.next_vaddr.checked_sub(size)?;
        if va < self.vaddr_floor {
            return None;
        }
        inner.next_vaddr = va;
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
        Some(DmaBuffer {
            phys,
            virt: va,
            size: size as usize,
        })
    }
}
