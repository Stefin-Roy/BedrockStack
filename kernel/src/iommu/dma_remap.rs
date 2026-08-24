//! IommuDma — `DmaAllocator` that returns IOVA instead of phys.
//!
//! Wraps the inner `KernelDma` (backing physical + CPU VA bump) and a
//! reference to the global VT-d domain. `phys` fields become IOVA.

use spin::Mutex;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::services::dma::{DmaAllocator, DmaBuffer};

use super::vt_d;

pub struct IommuDma {
    root: u64,
    inner_next_vaddr: Mutex<u64>,
    vaddr_floor: u64,
    inner_alloc: Mutex<*mut BitmapAllocator>,
}

unsafe impl Send for IommuDma {}
unsafe impl Sync for IommuDma {}

static IOMMU_DMA_GLOBAL: spin::Once<&'static IommuDma> = spin::Once::new();

pub fn set_global(dma: &'static IommuDma) {
    IOMMU_DMA_GLOBAL.call_once(|| dma);
}

pub fn update_alloc(alloc: *mut BitmapAllocator) {
    if let Some(dma) = IOMMU_DMA_GLOBAL.get() {
        *dma.inner_alloc.lock() = alloc;
    }
}

impl IommuDma {
    pub fn new(root: u64, alloc: *mut BitmapAllocator) -> Self {
        const DMA_VADDR_BASE: u64 = crate::mm::layout::DMA_VADDR_BASE;
        const DMA_VADDR_FLOOR: u64 = crate::mm::layout::DMA_VADDR_FLOOR;
        IommuDma {
            root,
            inner_next_vaddr: Mutex::new(DMA_VADDR_BASE),
            vaddr_floor: DMA_VADDR_FLOOR,
            inner_alloc: Mutex::new(alloc),
        }
    }

    pub fn update_alloc_self(&self, alloc: *mut BitmapAllocator) {
        *self.inner_alloc.lock() = alloc;
    }

    fn alloc_phys_contiguous(&self, count: usize) -> Option<(u64, usize)> {
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        let phys = alloc.alloc_contiguous(count)?;
        Some((phys, count * 4096))
    }

    fn alloc_phys_page(&self) -> Option<u64> {
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        alloc.alloc()
    }

    fn map_cpu(&self, va: u64, phys: u64, size: u64) {
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        Vmm::from_root(self.root).map(
            alloc,
            va,
            phys,
            size,
            crate::mm::vmm::PageFlags::READ
                | crate::mm::vmm::PageFlags::WRITE
                | crate::mm::vmm::PageFlags::NO_CACHE,
        );
    }
}

impl DmaAllocator for IommuDma {
    fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        let page_aligned = (size + 4095) & !4095;
        let mut next = self.inner_next_vaddr.lock();
        let va = next
            .checked_sub(page_aligned)
            .ok_or("IOMMU DMA: address space exhausted (overflow)")?;
        if va < self.vaddr_floor {
            return Err("IOMMU DMA: address space exhausted");
        }
        *next = va;
        self.map_cpu(va, paddr, page_aligned);
        Ok(va)
    }

    fn virt_to_phys(&self, vaddr: u64) -> Option<u64> {
        let vpage = vaddr & !0xFFF;
        let offset = vaddr & 0xFFF;
        let phys_page = Vmm::from_root(self.root).translate(vpage)?;
        let phys = (phys_page & !0xFFF) | offset;

        if !vt_d::is_enabled() {
            return Some(phys);
        }
        // If IOVA identity already (RMRR) return phys directly — RMRR is 1:1
        if let Some(mapped) = vt_d::translate_iova(phys & !0xFFF) {
            if mapped == (phys & !0xFFF) {
                return Some(phys);
            }
        }
        // For non-RMRR pages, allocate a fresh IOVA via the global domain.
        // No identity auto-map — per-DRHD DID isolation requires distinct IOVA.
        // Mappings are never reclaimed (leak-by-design, lifetime = kernel
        // uptime); the cache merely dedups repeat translations of one page.
        use alloc::collections::BTreeMap;
        use spin::Once;
        static PHYS_IOVA_CACHE: Once<spin::Mutex<BTreeMap<u64, u64>>> = Once::new();
        let cache = PHYS_IOVA_CACHE.call_once(|| spin::Mutex::new(BTreeMap::new()));
        {
            let guard = cache.lock();
            if let Some(&iova_page) = guard.get(&(phys & !0xFFF)) {
                return Some((iova_page & !0xFFF) | offset);
            }
        }
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        let phys_page_aligned = phys & !0xFFF;
        let iova_base = vt_d::map_phys_to_iova(phys_page_aligned, 4096, alloc)?;
        let iova_page = iova_base & !0xFFF;
        // Insert into cache (deduplicate). If race inserted first, prefer first.
        {
            let mut guard = cache.lock();
            guard.entry(phys_page_aligned).or_insert(iova_page);
            // Use cached value to ensure consistency
            let cached = *guard.get(&phys_page_aligned).unwrap();
            return Some((cached & !0xFFF) | offset);
        }
    }

    fn alloc_page(&self) -> Option<DmaBuffer> {
        let mut next = self.inner_next_vaddr.lock();
        let va = next.checked_sub(4096)?;
        if va < self.vaddr_floor {
            return None;
        }
        *next = va;
        drop(next);
        let phys = self.alloc_phys_page()?;
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        self.map_cpu(va, phys, 4096);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) }
        if !vt_d::is_enabled() {
            return Some(DmaBuffer {
                phys,
                virt: va,
                size: 4096,
            });
        }
        let iova = vt_d::map_phys_to_iova(phys, 4096, alloc)?;
        Some(DmaBuffer {
            phys: iova,
            virt: va,
            size: 4096,
        })
    }

    fn alloc_contiguous(&self, count: usize) -> Option<DmaBuffer> {
        let size = (count as u64) * 4096;
        let mut next = self.inner_next_vaddr.lock();
        let va = next.checked_sub(size)?;
        if va < self.vaddr_floor {
            return None;
        }
        *next = va;
        drop(next);
        let (phys, sz) = self.alloc_phys_contiguous(count)?;
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        self.map_cpu(va, phys, sz as u64);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, sz) }
        if !vt_d::is_enabled() {
            return Some(DmaBuffer {
                phys,
                virt: va,
                size: sz,
            });
        }
        let iova = vt_d::map_phys_to_iova(phys, size, alloc)?;
        Some(DmaBuffer {
            phys: iova,
            virt: va,
            size: sz,
        })
    }
}
