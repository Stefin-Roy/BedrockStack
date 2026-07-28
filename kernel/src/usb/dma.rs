use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

/// USB DMA VMM region: below AHCI VMM (0xFFFFFF7FB0000000).
/// AHCI is 1280 MiB below KERNEL_VMA_BASE; USB is another 1 GiB below that.
const USB_VMM_VADDR: u64 = 0xFFFFFF7F70000000;
const USB_VMM_VADDR_FLOOR: u64 = USB_VMM_VADDR - 0x2000_0000;

pub struct DmaBuffer {
    pub phys: u64,
    pub virt: u64,
    pub size: usize,
}

pub struct UsbDmaAllocator {
    root: u64,
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
    vaddr_floor: u64,
}

unsafe impl Send for UsbDmaAllocator {}

impl UsbDmaAllocator {
    pub fn new(root: u64, alloc: *mut BitmapAllocator) -> Self {
        UsbDmaAllocator {
            root,
            alloc,
            next_vaddr: USB_VMM_VADDR,
            vaddr_floor: USB_VMM_VADDR_FLOOR,
        }
    }

    pub fn map_mmio(&mut self, paddr: u64, size: u64) -> Result<u64, &'static str> {
        let page_aligned = (size + 4095) & !4095;
        let va = self
            .next_vaddr
            .checked_sub(page_aligned)
            .ok_or("USB DMA: address space exhausted (overflow)")?;
        if va < self.vaddr_floor {
            return Err("USB DMA: address space exhausted");
        }
        self.next_vaddr = va;
        let alloc = unsafe { &mut *self.alloc };
        Vmm::from_root(self.root).map(
            alloc,
            va,
            paddr,
            page_aligned,
            PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
        );
        Ok(va)
    }

    fn dma_flags() -> PageFlags {
        PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE
    }

    pub fn alloc_page(&mut self) -> Option<DmaBuffer> {
        let va = self.next_vaddr.checked_sub(4096)?;
        if va < self.vaddr_floor {
            return None;
        }
        self.next_vaddr = va;
        let alloc = unsafe { &mut *self.alloc };
        let phys = alloc.alloc()?;
        Vmm::from_root(self.root).map(alloc, va, phys, 4096, Self::dma_flags());
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) }
        Some(DmaBuffer {
            phys,
            virt: va,
            size: 4096,
        })
    }

    pub fn alloc_contiguous(&mut self, count: usize) -> Option<DmaBuffer> {
        let size = (count as u64) * 4096;
        let va = self.next_vaddr.checked_sub(size)?;
        if va < self.vaddr_floor {
            return None;
        }
        self.next_vaddr = va;
        let alloc = unsafe { &mut *self.alloc };
        let phys = alloc.alloc_contiguous(count)?;
        Vmm::from_root(self.root).map(alloc, va, phys, size, Self::dma_flags());
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, size as usize) }
        Some(DmaBuffer {
            phys,
            virt: va,
            size: size as usize,
        })
    }
}
