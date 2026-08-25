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
        if count == 0 || (count as u64).checked_mul(4096).is_none() {
            return None;
        }
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

    /// Remove a CPU mapping after a later IOVA setup step failed.  When VT-d
    /// is active the SLPT mapper has no transactional rollback yet, so the
    /// physical frames are deliberately retained rather than being returned
    /// while a partially-created IOVA could still reference them.
    fn rollback_cpu_mapping(&self, va: u64, size: u64, free_phys: bool) {
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        let mut vmm = Vmm::from_root(self.root);
        let pages = size / 4096;
        for i in 0..pages {
            let page_va = va + i * 4096;
            if let Some(pa) = vmm.translate(page_va) {
                vmm.unmap(alloc, page_va, 4096);
                if free_phys {
                    unsafe { alloc.free(pa & !0xFFF); }
                }
            }
        }
    }
}

impl DmaAllocator for IommuDma {
    fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        if size == 0 || paddr & 4095 != 0 {
            return Err("IOMMU DMA: MMIO address/size is not page-valid");
        }
        let page_aligned = size
            .checked_add(4095)
            .map(|v| v & !4095)
            .ok_or("IOMMU DMA: MMIO size overflow")?;
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
        let Some(iova) = vt_d::map_phys_to_iova(phys, 4096, alloc) else {
            self.rollback_cpu_mapping(va, 4096, false);
            return None;
        };
        Some(DmaBuffer {
            phys: iova,
            virt: va,
            size: 4096,
        })
    }

    fn alloc_contiguous(&self, count: usize) -> Option<DmaBuffer> {
        if count == 0 {
            return None;
        }
        let size = (count as u64).checked_mul(4096)?;
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
        let Some(iova) = vt_d::map_phys_to_iova(phys, size, alloc) else {
            self.rollback_cpu_mapping(va, size, false);
            return None;
        };
        Some(DmaBuffer {
            phys: iova,
            virt: va,
            size: sz,
        })
    }

    fn alloc_below4g(&self, count: usize) -> Option<DmaBuffer> {
        if count == 0 {
            return None;
        }
        let size = (count as u64).checked_mul(4096)?;
        // Reserve VA window (bump-only, same as alloc_page/contiguous).
        let va = {
            let mut next = self.inner_next_vaddr.lock();
            let va = next.checked_sub(size)?;
            if va < self.vaddr_floor {
                return None;
            }
            *next = va;
            va
        };
        // Physical backing must itself sit below 4 GiB when VT-d is off
        // (device sees phys), and we keep it low even when VT-d is on for
        // defense-in-depth.  Use the bounded allocator.
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        let Ok(phys) = alloc.try_alloc_contiguous_below(count, 0x1_0000_0000) else {
            // VA bump is intentionally leaked on failure (existing bump-only
            // semantics) — VA space is large and this path is cold.
            return None;
        };
        // Map into CPU VA space so the driver can touch the buffer.
        self.map_cpu(va, phys, size);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, size as usize) }
        if !vt_d::is_enabled() {
            return Some(DmaBuffer {
                phys,
                virt: va,
                size: size as usize,
            });
        }
        // With VT-d the device sees IOVA, so the IOVA itself must be < 4 GiB.
        let alloc_ptr2 = *self.inner_alloc.lock();
        let alloc2 = unsafe { &mut *alloc_ptr2 };
        let Some(iova) = vt_d::map_phys_to_iova_below(phys, size, alloc2, 0x1_0000_0000) else {
            // IOVA space below 4 GiB exhausted — tear down CPU mapping and
            // return the physical frames; VA bump remains leaked.
            let alloc_ptr3 = *self.inner_alloc.lock();
            let alloc3 = unsafe { &mut *alloc_ptr3 };
            let mut vmm = Vmm::from_root(self.root);
            let pages = size.div_ceil(4096);
            for i in 0..pages {
                let v = va + i * 4096;
                if vmm.translate(v).is_some() {
                    vmm.unmap(alloc3, v, 4096);
                }
            }
            // Free contiguous physical backing frame-by-frame (free() guards
            // frame 0 / kernel range internally; below-4G frames are never
            // those).
            // If VT-d was enabled, map_phys_to_iova may have installed a
            // prefix before failing; retaining the frames is safer than
            // allowing a stale IOVA to target a future owner.
            if !vt_d::is_enabled() {
                for i in 0..count as u64 {
                    unsafe { alloc3.free(phys + i * 4096) };
                }
            }
            return None;
        };
        Some(DmaBuffer {
            phys: iova,
            virt: va,
            size: size as usize,
        })
    }

    fn free(&self, buf: &DmaBuffer) {
        if buf.virt == 0 || buf.size == 0 || buf.size % 4096 != 0 {
            return;
        }
        // Without a `vt_d::unmap_iova` there is no safe way to retire a
        // buffer under VT-d: releasing its frames while the IOVA second-level
        // mapping persists would let an in-flight/stale device write land in
        // reused memory.  So: VT-d off → full teardown (CPU unmap + frames);
        // VT-d on → deliberate leak (status quo) with a one-line WARN.
        if vt_d::is_enabled() {
            crate::drivers::serial::SerialPort::puts(
                "[dma] WARN: IommuDma::free leaks buffer (no vt_d::unmap_iova yet)\n",
            );
            return;
        }
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        let mut vmm = Vmm::from_root(self.root);
        for i in 0..(buf.size as u64).div_ceil(4096) {
            let va = buf.virt + i * 4096;
            if let Some(pa) = vmm.translate(va) {
                vmm.unmap(alloc, va, 4096);
                unsafe { alloc.free(pa & !0xFFF) };
            }
        }
    }
}
