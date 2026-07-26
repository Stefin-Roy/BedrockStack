use crate::filesystems::vfs::irq::IrqMutex;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

const TRANS_CACHE_SIZE: usize = 64;

struct TransCacheInner {
    entries: [(u64, u64); TRANS_CACHE_SIZE],
    next: usize,
}

struct TransCache {
    inner: IrqMutex<TransCacheInner>,
}

impl TransCache {
    const fn new() -> Self {
        TransCache {
            inner: IrqMutex::new(TransCacheInner {
                entries: [(0, 0); TRANS_CACHE_SIZE],
                next: 0,
            }),
        }
    }

    fn lookup_or_translate(&self, vaddr: u64, root: u64) -> Option<u64> {
        let mut inner = self.inner.lock();
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
        let va = self.next_vaddr.checked_sub(4096)?;
        if va < self.vaddr_floor {
            return None;
        }
        self.next_vaddr = va;
        Vmm::from_root(self.root).map(alloc, va, phys, 4096,
            PageFlags::READ | PageFlags::WRITE);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096); }
        Some(DmaBuffer { phys, virt: va, size: 4096 })
    }

    /// Allocate `count` contiguous physical pages, map them into DMA
    /// virtual address space, zero them, and return a single DmaBuffer.
    ///
    /// NVMe needs this for Submission/Completion Queues and PRP lists.
    /// XHCI can use it for Transfer Request Block rings.
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<DmaBuffer> {
        let alloc = unsafe { &mut *self.alloc };
        let phys = alloc.alloc_contiguous(count)?;
        let size = (count as u64) * 4096;
        let va = self.next_vaddr.checked_sub(size)?;
        if va < self.vaddr_floor {
            return None;
        }
        self.next_vaddr = va;
        Vmm::from_root(self.root).map(alloc, va, phys, size,
            PageFlags::READ | PageFlags::WRITE);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, size as usize); }
        Some(DmaBuffer { phys, virt: va, size: size as usize })
    }
}

pub fn translate(root: u64, vaddr: u64) -> Option<u64> {
    TRANS_CACHE.lookup_or_translate(vaddr, root)
}
