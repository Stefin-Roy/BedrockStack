use core::arch::asm;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::KernelLayout;

const PAGE_SIZE: u64 = 4096;
const PAGE_2M: u64 = 512 * 4096;
const PAGE_1G: u64 = 512 * 512 * 4096;

const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_G: u64 = 1 << 5;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

const SATP_MODE_SV39: u64 = 8 << 60;

#[repr(C, align(4096))]
struct PageTable {
    entries: [PageTableEntry; 512],
}

#[derive(Clone, Copy)]
struct PageTableEntry(u64);

impl PageTableEntry {
    fn new(ppn: u64, flags: u64) -> Self {
        PageTableEntry((ppn << 10) | flags)
    }
    fn is_valid(self) -> bool {
        self.0 & PTE_V != 0
    }
    fn ppn(self) -> u64 {
        self.0 >> 10
    }
}

fn paddr_to_ppn(paddr: u64) -> u64 {
    paddr >> 12
}

fn ppn_to_paddr(ppn: u64) -> u64 {
    ppn << 12
}

fn vpn_index(vaddr: u64, level: usize) -> usize {
    ((vaddr >> (12 + level * 9)) & 0x1FF) as usize
}

fn pt_at_mut<'a>(ppn: u64) -> &'a mut PageTable {
    unsafe { &mut *(ppn_to_paddr(ppn) as *mut PageTable) }
}

fn alloc_pt(allocator: &mut BitmapAllocator) -> (u64, &'static mut PageTable) {
    let phys = allocator.alloc().expect("OOM for page table");
    let pt = unsafe { &mut *(phys as *mut PageTable) };
    pt.entries.fill(PageTableEntry(0));
    (phys, pt)
}

fn map_4k(root: &mut PageTable, allocator: &mut BitmapAllocator, vaddr: u64, paddr: u64, flags: u64) {
    let idx2 = vpn_index(vaddr, 2);
    let idx1 = vpn_index(vaddr, 1);
    let idx0 = vpn_index(vaddr, 0);

    if !root.entries[idx2].is_valid() {
        let (phys, pt) = alloc_pt(allocator);
        root.entries[idx2] = PageTableEntry::new(paddr_to_ppn(phys), PTE_V);
        let l2 = pt;
        let (phys2, l1) = alloc_pt(allocator);
        l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(phys2), PTE_V);
        l1.entries[idx0] = PageTableEntry::new(paddr_to_ppn(paddr), flags | PTE_V | PTE_A | PTE_D);
    } else {
        let l2 = pt_at_mut(root.entries[idx2].ppn());
        if !l2.entries[idx1].is_valid() {
            let (phys, l1) = alloc_pt(allocator);
            l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(phys), PTE_V);
            l1.entries[idx0] = PageTableEntry::new(paddr_to_ppn(paddr), flags | PTE_V | PTE_A | PTE_D);
        } else {
            let l1 = pt_at_mut(l2.entries[idx1].ppn());
            l1.entries[idx0] = PageTableEntry::new(paddr_to_ppn(paddr), flags | PTE_V | PTE_A | PTE_D);
        }
    }
}

fn map_2m(root: &mut PageTable, allocator: &mut BitmapAllocator, vaddr: u64, paddr: u64, mut flags: u64) {
    let idx2 = vpn_index(vaddr, 2);
    let idx1 = vpn_index(vaddr, 1);

    flags |= PTE_V | PTE_A | PTE_D;

    if !root.entries[idx2].is_valid() {
        let (phys, l2) = alloc_pt(allocator);
        root.entries[idx2] = PageTableEntry::new(paddr_to_ppn(phys), PTE_V);
        l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(paddr), flags);
    } else {
        let l2 = pt_at_mut(root.entries[idx2].ppn());
        l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(paddr), flags);
    }
}

fn leaf_flags_4k(addr: u64, layout: &KernelLayout, fb_start: u64, fb_end: u64) -> u64 {
    let mut flags = PTE_V | PTE_A | PTE_D;
    if addr >= layout.text_start && addr < layout.text_end {
        flags |= PTE_R | PTE_X;
    } else if addr >= layout.rodata_start && addr < layout.rodata_end {
        flags |= PTE_R;
    } else {
        flags |= PTE_R | PTE_W;
    }
    if addr >= fb_start && addr < fb_end {
        flags &= !PTE_X;
    } else if !(addr >= layout.text_start && addr < layout.text_end) {
        flags &= !PTE_X;
    }
    flags
}

pub fn setup(
    allocator: &mut BitmapAllocator,
    layout: &KernelLayout,
    stack_guard: u64,
    framebuffer_addr: u64,
    framebuffer_height: usize,
    framebuffer_stride: usize,
) {
    let fb_size = (framebuffer_stride * framebuffer_height * 4) as u64;
    let fb_start = framebuffer_addr;
    let fb_end = framebuffer_addr.saturating_add(fb_size);

    let min_end = 4u64 * 1024 * 1024 * 1024;
    let max_addr = fb_end.max(min_end).max(allocator.alloc_end());
    let max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let (root_phys, root) = alloc_pt(allocator);

    let mut chunk = 0u64;
    while chunk < max_addr {
        let chunk_end = chunk + PAGE_2M;
        let overlaps_kernel = chunk < layout.kernel_end && chunk_end > layout.kernel_start;
        let contains_guard = stack_guard != 0 && stack_guard >= chunk && stack_guard < chunk_end;
        let is_first = chunk == 0;

        if overlaps_kernel || contains_guard || is_first {
            let mut page_addr = chunk;
            while page_addr < chunk_end {
                if page_addr == 0 || (stack_guard != 0 && page_addr == stack_guard) {
                    page_addr += PAGE_SIZE;
                    continue;
                }
                let flags = leaf_flags_4k(page_addr, layout, fb_start, fb_end);
                map_4k(root, allocator, page_addr, page_addr, flags);
                page_addr += PAGE_SIZE;
            }
        } else {
            map_2m(root, allocator, chunk, chunk, PTE_R | PTE_W);
        }
        chunk = chunk_end;
    }

    unsafe {
        let satp = SATP_MODE_SV39 | paddr_to_ppn(root_phys);
        asm!("csrw satp, {}", in(reg) satp);
        asm!("sfence.vma");
    }
}
