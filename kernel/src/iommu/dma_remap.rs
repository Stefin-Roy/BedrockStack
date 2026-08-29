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

/// Fixed-capacity VA free list — mirrors `KernelDma::FreeWindows:132` for
/// CPU VA reclaim. Bump cursor stays monotonic; freed windows are served
/// first. No heap, safe under lock.
struct FreeWindows {
    entries: [(u64, u64); 64],
    len: usize,
}
impl FreeWindows {
    const fn new() -> Self {
        FreeWindows {
            entries: [(0, 0); 64],
            len: 0,
        }
    }
    fn push(&mut self, va: u64, size: u64) {
        if self.len < self.entries.len() {
            self.entries[self.len] = (va, size);
            self.len += 1;
        }
    }
    fn take_exact(&mut self, size: u64) -> Option<u64> {
        for i in 0..self.len {
            if self.entries[i].1 == size {
                let va = self.entries[i].0;
                self.entries[i] = self.entries[self.len - 1];
                self.len -= 1;
                return Some(va);
            }
        }
        None
    }
}

struct InnerVa {
    next_vaddr: u64,
    free_windows: FreeWindows,
}

pub struct IommuDma {
    root: u64,
    inner_va: Mutex<InnerVa>,
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
            inner_va: Mutex::new(InnerVa {
                next_vaddr: DMA_VADDR_BASE,
                free_windows: FreeWindows::new(),
            }),
            vaddr_floor: DMA_VADDR_FLOOR,
            inner_alloc: Mutex::new(alloc),
        }
    }

    fn reserve_window(&self, size: u64) -> Option<u64> {
        let mut inner = self.inner_va.lock();
        if let Some(va) = inner.free_windows.take_exact(size) {
            return Some(va);
        }
        let va = inner.next_vaddr.checked_sub(size)?;
        if va < self.vaddr_floor {
            return None;
        }
        inner.next_vaddr = va;
        Some(va)
    }

    fn reclaim_window(&self, va: u64, size: u64) {
        let mut inner = self.inner_va.lock();
        inner.free_windows.push(va, size);
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
        let va = {
            let mut inner = self.inner_va.lock();
            let va = inner
                .next_vaddr
                .checked_sub(page_aligned)
                .ok_or("IOMMU DMA: address space exhausted (overflow)")?;
            if va < self.vaddr_floor {
                return Err("IOMMU DMA: address space exhausted");
            }
            inner.next_vaddr = va;
            va
        };
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
            // Prefer the first inserted value for consistency; reclaim the duplicate
            // IOVA via `unmap_iova` so it returns to `FreeIovaWindows`.
            let entry = guard.entry(phys_page_aligned).or_insert(iova_page);
            let cached = *entry;
            if cached != iova_page {
                // Lost race — reclaim the duplicate we just allocated.
                drop(guard);
                let _ = vt_d::unmap_iova(iova_page, 4096);
                return Some((cached & !0xFFF) | offset);
            }
            return Some((cached & !0xFFF) | offset);
        }
    }

    fn alloc_page(&self) -> Option<DmaBuffer> {
        let va = self.reserve_window(4096)?;
        let phys = match self.alloc_phys_page() {
            Some(p) => p,
            None => {
                self.reclaim_window(va, 4096);
                return None;
            }
        };
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
            self.reclaim_window(va, 4096);
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
        let va = self.reserve_window(size)?;
        // Use dedicated helper for phys allocation (actually uses inner_alloc lock internally).
        let (phys, sz) = match self.alloc_phys_contiguous(count) {
            Some(v) => v,
            None => {
                self.reclaim_window(va, size);
                return None;
            }
        };
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
            self.reclaim_window(va, size);
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
        // Reserve VA window via free list or bump.
        let va = self.reserve_window(size)?;
        // Physical backing must itself sit below 4 GiB when VT-d is off
        // (device sees phys), and we keep it low even when VT-d is on for
        // defense-in-depth. Use the bounded allocator (scoped to avoid holding across map).
        let phys = {
            let alloc_ptr = *self.inner_alloc.lock();
            let alloc = unsafe { &mut *alloc_ptr };
            match alloc.try_alloc_contiguous_below(count, 0x1_0000_0000) {
                Ok(p) => p,
                Err(_) => {
                    self.reclaim_window(va, size);
                    return None;
                }
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
            self.reclaim_window(va, size);
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
        // VT-d on: full reclamation — SLPT unmap → IOVA VA reuse via
        // Domain::free_iova, PHYS_IOVA_CACHE evict, CPU VA unmap + frame
        // free, DMA VA reuse via InnerVa::free_windows. Silent, no WARN.
        // Keeps `next_iova` bump for limit edge but reclaimed VA reused via
        // `take` → NVMe 32× scan no longer marches to `iova_limit`.
        if vt_d::is_enabled() {
            let iova_base = buf.phys & !0xFFF;
            let size_aligned = (buf.size as u64 + 0xFFF) & !0xFFF;
            let pages = (size_aligned / 4096) as usize;
            let mut phys_pages = alloc::vec::Vec::with_capacity(pages);
            for i in 0..pages as u64 {
                let iova_page = iova_base + i * 4096;
                if let Some(phys) = crate::iommu::vt_d::translate_iova(iova_page) {
                    phys_pages.push(phys & !0xFFF);
                }
            }
            // Unmap IOVA range — pushes to Domain::free_iova for reuse and
            // invalidates IOTLB on all enabled units. Even if `phys_pages`
            // is empty (double-free or RMRR hole) we still reclaim IOVA VA
            // so the bump allocator does not leak forever.
            let _ = crate::iommu::vt_d::unmap_iova(iova_base, size_aligned);
            if !phys_pages.is_empty() {
                let mut cache = phys_iova_cache().lock();
                for phys in &phys_pages {
                    cache.remove(phys);
                }
            }
            // Unmap CPU VA and free backing frames.
            let alloc_ptr = *self.inner_alloc.lock();
            let alloc = unsafe { &mut *alloc_ptr };
            let mut vmm = Vmm::from_root(self.root);
            let mut freed_any = false;
            for i in 0..pages as u64 {
                let va = buf.virt + i * 4096;
                if let Some(pa) = vmm.translate(va) {
                    vmm.unmap(alloc, va, 4096);
                    // Prefer phys recovered from IOVA translation (authoritative
                    // backing), fall back to VMM pa if translate missed.
                    let phys_to_free = if (i as usize) < phys_pages.len() {
                        phys_pages[i as usize]
                    } else {
                        pa & !0xFFF
                    };
                    unsafe { alloc.free(phys_to_free) };
                    freed_any = true;
                }
            }
            // Reclaim DMA VA window if we actually unmapped something —
            // guards double-free from creating duplicate windows that would
            // alias future allocations. Matches `KernelDma::free` discipline.
            if freed_any {
                self.reclaim_window(buf.virt, buf.size as u64);
            } else if pages > 0 {
                // No CPU mapping existed but IOVA was reclaimed above.
                // Still reclaim VA if it was a valid window (e.g., phys-only
                // mapping), but only if the VA was previously reserved.
                // Conservative: do not push unless we know VA was ours.
                // The early `vmm.translate` failure already indicates double
                // free or stale buf, so skip to avoid alias.
            }
            return;
        }
        let alloc_ptr = *self.inner_alloc.lock();
        let alloc = unsafe { &mut *alloc_ptr };
        let mut vmm = Vmm::from_root(self.root);
        let mut freed_any = false;
        for i in 0..(buf.size as u64).div_ceil(4096) {
            let va = buf.virt + i * 4096;
            if let Some(pa) = vmm.translate(va) {
                vmm.unmap(alloc, va, 4096);
                unsafe { alloc.free(pa & !0xFFF) };
                freed_any = true;
            }
        }
        if freed_any {
            self.reclaim_window(buf.virt, buf.size as u64);
        }
    }
}
