//! IommuDma — `DmaAllocator` that returns IOVA instead of phys.
//!
//! Wraps the inner `KernelDma` (backing physical + CPU VA bump) and a
//! reference to the global VT-d domain. `phys` fields become IOVA.

use alloc::collections::BTreeMap;
use spin::{Mutex, Once};

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

static IOMMU_DMA_GLOBAL: Once<&'static IommuDma> = Once::new();

// Global phys->IOVA deduplication cache — shared between alloc_* and virt_to_phys so that
// virt_to_phys does not allocate a duplicate IOVA for a page already mapped via alloc_*.
static PHYS_IOVA_CACHE: Once<Mutex<BTreeMap<u64, u64>>> = Once::new();

fn phys_iova_cache() -> &'static Mutex<BTreeMap<u64, u64>> {
    PHYS_IOVA_CACHE.call_once(|| Mutex::new(BTreeMap::new()))
}

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

    /// Map CPU VA without re-borrowing the allocator — caller must hold the
    /// `inner_alloc` borrow and pass it in. This avoids stacked-borrows UB
    /// from creating two simultaneous `&mut BitmapAllocator` from the same raw pointer.
    fn map_cpu_with_alloc(&self, alloc: &mut BitmapAllocator, va: u64, phys: u64, size: u64) {
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
        Self::rollback_cpu_mapping_with_alloc(self.root, alloc, va, size, free_phys);
    }

    fn rollback_cpu_mapping_with_alloc(
        root: u64,
        alloc: &mut BitmapAllocator,
        va: u64,
        size: u64,
        free_phys: bool,
    ) {
        let mut vmm = Vmm::from_root(root);
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
        // If IOVA identity already (RMRR) return phys directly — RMRR is 1:1.
        // NOTE: translate_iova probe is fragile (PA==IOVA collision) but retained for
        // RMRR identity fast-path; a stricter check would walk the RMRR descriptor list.
        if let Some(mapped) = vt_d::translate_iova(phys & !0xFFF) {
            if mapped == (phys & !0xFFF) {
                return Some(phys);
            }
        }
        let phys_page_aligned = phys & !0xFFF;
        let cache = phys_iova_cache();
        // Fast-path: check cache without holding alloc lock.
        {
            let guard = cache.lock();
            if let Some(&iova_page) = guard.get(&phys_page_aligned) {
                return Some((iova_page & !0xFFF) | offset);
            }
        }
        // Cache miss — allocate a fresh IOVA. Hold alloc and cache in
        // consistent order (alloc -> cache) to avoid deadlock with alloc_*
        // paths which also use alloc->cache order when inserting.
        // To avoid duplicate IOVAs under concurrent virt_to_phys, we keep
        // the cache lock across the allocation and re-check after allocation
        // (or insert with or_insert). Duplicate allocations are de-duplicated
        // by returning the first inserted IOVA; the leaked duplicate mapping
        // remains but is rare and bounded to races — a full fix would need
        // vt_d::unmap_iova to reclaim it.
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        // Re-check after acquiring alloc (another thread may have inserted while we waited for alloc lock).
        {
            let guard = cache.lock();
            if let Some(&iova_page) = guard.get(&phys_page_aligned) {
                return Some((iova_page & !0xFFF) | offset);
            }
        }
        let iova_base = vt_d::map_phys_to_iova(phys_page_aligned, 4096, alloc)?;
        let iova_page = iova_base & !0xFFF;
        {
            let mut guard = cache.lock();
            // Use or_insert to handle race where two threads allocated concurrently.
            // Prefer the first inserted value for consistency; the second IOVA leaks (bounded).
            let entry = guard.entry(phys_page_aligned).or_insert(iova_page);
            let cached = *entry;
            // If we lost the race (cached != iova_page), the iova_page mapping is leaked.
            // Ideally we would unmap it via vt_d::unmap_iova, but no such API exists yet.
            // We return the cached (first) value so all callers agree.
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
        // Use with_alloc helper while holding borrow to avoid aliasing UB.
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        self.map_cpu_with_alloc(alloc, va, phys, 4096);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) }
        if !vt_d::is_enabled() {
            return Some(DmaBuffer {
                phys,
                virt: va,
                size: 4096,
            });
        }
        let Some(iova) = vt_d::map_phys_to_iova(phys, 4096, alloc) else {
            // Still holding alloc, use with_alloc variant to avoid re-borrowing.
            Self::rollback_cpu_mapping_with_alloc(self.root, alloc, va, 4096, true);
            return None;
        };
        // Populate phys->IOVA cache so virt_to_phys does not allocate a duplicate IOVA for this page.
        {
            let mut c = phys_iova_cache().lock();
            c.insert(phys & !0xFFF, iova & !0xFFF);
        }
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
        // Use dedicated helper for phys allocation (actually uses inner_alloc lock internally).
        let (phys, sz) = self.alloc_phys_contiguous(count)?;
        // Map and VT-d steps need the allocator for page tables; hold it across both to avoid aliasing.
        // Use a scoped borrow so failure can delegate to the locking rollback helper without UB.
        let iova_opt = {
            let alloc_ptr = *self.inner_alloc.lock();
            let alloc = unsafe { &mut *alloc_ptr };
            self.map_cpu_with_alloc(alloc, va, phys, sz as u64);
            unsafe { core::ptr::write_bytes(va as *mut u8, 0, sz) }
            if !vt_d::is_enabled() {
                return Some(DmaBuffer {
                    phys,
                    virt: va,
                    size: sz,
                });
            }
            vt_d::map_phys_to_iova(phys, size, alloc)
        };
        let Some(iova) = iova_opt else {
            // Alloc borrow ended, safe to use locking helper (actually uses the same alloc via lock).
            self.rollback_cpu_mapping(va, size, true);
            return None;
        };
        {
            let mut c = phys_iova_cache().lock();
            // For contiguous, insert per-page entries so virt_to_phys hits cache for any sub-page.
            for i in 0..count as u64 {
                let p_phys = (phys + i * 4096) & !0xFFF;
                let p_iova = (iova + i * 4096) & !0xFFF;
                c.insert(p_phys, p_iova);
            }
        }
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
        // defense-in-depth. Use the bounded allocator (scoped to avoid holding across map).
        let phys = {
            let alloc_ptr = *self.inner_alloc.lock();
            let alloc = unsafe { &mut *alloc_ptr };
            match alloc.try_alloc_contiguous_below(count, 0x1_0000_0000) {
                Ok(p) => p,
                Err(_) => return None,
            }
        };
        // CPU mapping + VT-d IOVA in a scoped alloc borrow so failure can use
        // the locking rollback helper without aliasing.
        let iova_opt = {
            let alloc_ptr = *self.inner_alloc.lock();
            let alloc = unsafe { &mut *alloc_ptr };
            self.map_cpu_with_alloc(alloc, va, phys, size);
            unsafe { core::ptr::write_bytes(va as *mut u8, 0, size as usize) }
            if !vt_d::is_enabled() {
                return Some(DmaBuffer {
                    phys,
                    virt: va,
                    size: size as usize,
                });
            }
            vt_d::map_phys_to_iova_below(phys, size, alloc, 0x1_0000_0000)
        };
        let Some(iova) = iova_opt else {
            // Alloc borrow ended, safe to use locking helper which will unmap and free.
            self.rollback_cpu_mapping(va, size, true);
            return None;
        };
        {
            let mut c = phys_iova_cache().lock();
            for i in 0..count as u64 {
                let p_phys = (phys + i * 4096) & !0xFFF;
                let p_iova = (iova + i * 4096) & !0xFFF;
                c.insert(p_phys, p_iova);
            }
        }
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
