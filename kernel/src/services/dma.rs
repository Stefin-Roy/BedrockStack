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
    /// Release a buffer previously returned by [`Self::alloc_page`] /
    /// [`Self::alloc_contiguous`].
    ///
    /// Default implementation intentionally does nothing (legacy leak
    /// behaviour) so existing drivers stay correct until they adopt the call.
    /// Implementors that can safely tear down should override.
    fn free(&self, _buf: &DmaBuffer) {}
    /// Allocate `count` pages whose backing frames lie entirely in the 32-bit
    /// DMA zone (< 4 GiB), for controllers without 64-bit addressing.
    ///
    /// Default ignores the constraint (legacy behaviour); [`KernelDma`]
    /// honors it via the allocator's bounded-span search.
    fn alloc_below4g(&self, count: usize) -> Option<DmaBuffer> {
        if count == 1 {
            self.alloc_page()
        } else {
            self.alloc_contiguous(count)
        }
    }
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
    #[cfg(target_arch = "x86_64")]
    crate::iommu::update_alloc(alloc);
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
        let Some(end) = vaddr
            .checked_add(size)
            .and_then(|v| v.checked_add(0xFFF))
            .map(|v| v & !0xFFF)
        else {
            // A wrapped invalidation range cannot be represented.  Drop the
            // whole small cache instead of retaining translations that may
            // alias a future DMA mapping.
            for (v, _) in self.entries.iter_mut() {
                *v = 0;
            }
            return;
        };
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
    /// Freed VA windows available for reuse.  The bump cursor stays
    /// monotonic; freed windows are served first so a driver that releases
    /// buffers stops marching toward the arena floor.  Fixed capacity and
    /// no heap allocation — safe under the DMA lock; when full, windows are
    /// dropped (VA-space leak only, never frames).
    free_windows: FreeWindows,
}

/// Fixed-capacity `(vaddr, size)` list.
struct FreeWindows {
    entries: [(u64, u64); 64],
    len: usize,
}

impl FreeWindows {
    const fn new() -> Self {
        FreeWindows { entries: [(0, 0); 64], len: 0 }
    }
    fn push(&mut self, va: u64, size: u64) {
        if self.len < self.entries.len() {
            self.entries[self.len] = (va, size);
            self.len += 1;
        }
    }
    /// Exact-fit take, or `None`.
    fn take_exact(&mut self, size: u64) -> Option<u64> {
        for i in 0..self.len {
            if self.entries[i].1 == size {
                let va = self.entries[i].0;
                self.len -= 1;
                self.entries[i] = self.entries[self.len];
                return Some(va);
            }
        }
        None
    }
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
                free_windows: FreeWindows::new(),
            }),
        }
    }

    /// Reserve a VA window of `size` bytes: a freed exact-fit window when
    /// available, else a fresh downward bump.
    fn reserve_window(&self, inner: &mut DmaInner, size: u64) -> Option<u64> {
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
}

impl DmaAllocator for KernelDma {
    fn map_mmio(&self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        if size == 0 || paddr & 4095 != 0 {
            return Err("DMA: MMIO address/size is not page-valid");
        }
        let page_aligned = size
            .checked_add(4095)
            .map(|v| v & !4095)
            .ok_or("DMA: MMIO size overflow")?;
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
        let va = self.reserve_window(&mut inner, 4096)?;
        let alloc = unsafe { &mut *inner.alloc };
        let Some(phys) = alloc.alloc() else {
            // Physical allocation failed — return VA window to the free list
            // so a later free can reuse it instead of leaking bump space.
            inner.free_windows.push(va, 4096);
            return None;
        };
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
        let size = (count as u64).checked_mul(4096)?;
        if count == 0 {
            return None;
        }
        let mut inner = self.inner.lock();
        let va = self.reserve_window(&mut inner, size)?;
        let alloc = unsafe { &mut *inner.alloc };
        let Some(phys) = alloc.alloc_contiguous(count) else {
            inner.free_windows.push(va, size);
            return None;
        };
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

    fn free(&self, buf: &DmaBuffer) {
        if buf.virt == 0 || buf.size == 0 || buf.size % 4096 != 0 {
            return;
        }
        let mut inner = self.inner.lock();
        // Recover each frame through the still-live mapping, tear the
        // mapping down (unmap + shootdown), then release the frames.  Only
        // push the VA window back if we actually unmapped something — a
        // double-free would otherwise create duplicate windows and two future
        // allocations would alias the same VA.
        let alloc = unsafe { &mut *inner.alloc };
        let mut vmm = Vmm::from_root(self.root);
        let pages = buf.size.div_ceil(4096);
        let mut freed_any = false;
        for i in 0..pages as u64 {
            let va = buf.virt + i * 4096;
            if let Some(pa) = vmm.translate(va) {
                vmm.unmap(alloc, va, 4096);
                unsafe { alloc.free(pa & !0xFFF) };
                freed_any = true;
            }
        }
        if freed_any {
            inner.free_windows.push(buf.virt, buf.size as u64);
            // Invalidate translation cache for the freed range so a later reuse of the same VA
            // does not return a stale phys via virt_to_phys (which is used for PRP building).
            TRANS_CACHE.lock().invalidate_range(buf.virt, buf.size as u64);
        }
    }

    fn alloc_below4g(&self, count: usize) -> Option<DmaBuffer> {
        let size = (count as u64).checked_mul(4096)?;
        if count == 0 {
            return None;
        }
        let mut inner = self.inner.lock();
        let va = self.reserve_window(&mut inner, size)?;
        let alloc = unsafe { &mut *inner.alloc };
        let Ok(phys) = alloc.try_alloc_contiguous_below(count, 0x1_0000_0000) else {
            inner.free_windows.push(va, size);
            return None;
        };
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
