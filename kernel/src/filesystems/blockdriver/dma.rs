use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

const TRANS_CACHE_SIZE: usize = 64;

struct TransCacheInner {
    entries: [(u64, u64); TRANS_CACHE_SIZE],
    next: usize,
}

struct TransCache {
    data: core::cell::UnsafeCell<TransCacheInner>,
}

unsafe impl Sync for TransCache {}

impl TransCache {
    const fn new() -> Self {
        TransCache {
            data: core::cell::UnsafeCell::new(TransCacheInner {
                entries: [(0, 0); TRANS_CACHE_SIZE],
                next: 0,
            }),
        }
    }

    fn lookup_or_translate(&self, vaddr: u64, root: u64) -> Option<u64> {
        let inner = unsafe { &mut *self.data.get() };
        let vaddr_page = vaddr & !0xFFF;
        for &(v, p) in &inner.entries {
            if v == vaddr_page {
                return Some(p);
            }
        }
        let pa = Vmm::from_root(root).translate(vaddr_page)?;
        let idx = inner.next % TRANS_CACHE_SIZE;
        inner.entries[idx] = (vaddr_page, pa);
        inner.next = inner.next.wrapping_add(1);
        Some(pa)
    }
}

static TRANS_CACHE: TransCache = TransCache::new();

pub struct DmaBuffer {
    pub phys: u64,
    pub virt: u64,
    pub size: usize,
}

pub struct DmaAllocator {
    root: u64,
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
    vaddr_floor: u64,
}

unsafe impl Send for DmaAllocator {}
unsafe impl Sync for DmaAllocator {}

impl DmaAllocator {
    pub fn new(root: u64, alloc: *mut BitmapAllocator, mmio_start: u64, mmio_floor: u64) -> Self {
        DmaAllocator { root, alloc, next_vaddr: mmio_start, vaddr_floor: mmio_floor }
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    pub fn map_mmio(&mut self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        let va = self.next_vaddr.checked_sub(size).ok_or("DMA: address space exhausted (overflow)")?;
        if va < self.vaddr_floor {
            return Err("DMA: address space exhausted");
        }
        self.next_vaddr = va;
        let alloc = unsafe { &mut *self.alloc };
        Vmm::from_root(self.root).map(alloc, va, paddr, size, PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE);
        Ok(va)
    }

    pub fn virt_to_phys(&self, vaddr: u64) -> Option<u64> {
        TRANS_CACHE.lookup_or_translate(vaddr, self.root)
    }

    pub fn alloc_page(&mut self) -> Option<DmaBuffer> {
        let alloc = unsafe { &mut *self.alloc };
        let phys = alloc.alloc()?;
        unsafe { core::ptr::write_bytes(phys as *mut u8, 0, 4096); }
        Some(DmaBuffer { phys, virt: phys, size: 4096 })
    }
}

pub fn translate(root: u64, vaddr: u64) -> Option<u64> {
    TRANS_CACHE.lookup_or_translate(vaddr, root)
}
