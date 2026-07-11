//! RISC-V Sv39 page table operations (hand-rolled).
//!
//! Sv39 is a three-level page table:
//!   Level 2 (top):  index bits 38:30  →  512 entries
//!   Level 1 (mid):  index bits 29:21  →  512 entries
//!   Level 0 (leaf): index bits 20:12  →  512 entries (or 2 MiB megapage at L1)

use core::arch::asm;
use crate::mm::phys_alloc::BitmapAllocator;
use super::PageFlags;

// ── Page size constants ──────────────────────────────────────────────
const _PAGE_SIZE: u64 = 4096;
const _PAGE_2M: u64 = 512 * 4096;

// ── Sv39 PTE flags ──────────────────────────────────────────────────
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

const SATP_MODE_SV39: u64 = 8 << 60;

// ── Page table type ──────────────────────────────────────────────────

#[repr(C, align(4096))]
struct PageTable {
    entries: [PageTableEntry; 512],
}

#[derive(Clone, Copy)]
struct PageTableEntry(u64);

impl PageTableEntry {
    fn is_valid(self) -> bool {
        self.0 & PTE_V != 0
    }
    fn ppn(self) -> u64 {
        self.0 >> 10
    }
    fn new(ppn: u64, flags: u64) -> Self {
        PageTableEntry((ppn << 10) | flags)
    }
    fn clear(&mut self) {
        self.0 = 0;
    }
}

// ── Address helpers ──────────────────────────────────────────────────

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
fn pt_at<'a>(ppn: u64) -> &'a PageTable {
    unsafe { &*(ppn_to_paddr(ppn) as *const PageTable) }
}
fn alloc_pt(alloc: &mut BitmapAllocator) -> (u64, &'static mut PageTable) {
    let phys = alloc.alloc().expect("riscv64 VMM: OOM for page table");
    let pt = unsafe { &mut *(phys as *mut PageTable) };
    pt.entries.fill(PageTableEntry(0));
    (phys, pt)
}

// ── Flag conversion ──────────────────────────────────────────────────

fn page_flags_to_riscv(flags: PageFlags) -> u64 {
    let mut f = PTE_V | PTE_A | PTE_D;
    if flags.contains(PageFlags::READ) {
        f |= PTE_R;
    }
    if flags.contains(PageFlags::WRITE) {
        f |= PTE_W;
    }
    if flags.contains(PageFlags::EXECUTE) {
        f |= PTE_X;
    }
    if flags.contains(PageFlags::USER) {
        f |= PTE_U;
    }
    f
}

// ── Public API ──────────────────────────────────────────────────────

pub fn map_4k(
    root: u64,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    paddr: u64,
    flags: PageFlags,
) {
    let root_pt = unsafe { &mut *(root as *mut PageTable) };
    let rf = page_flags_to_riscv(flags);

    let idx2 = vpn_index(vaddr, 2);
    let idx1 = vpn_index(vaddr, 1);
    let idx0 = vpn_index(vaddr, 0);

    // Level 2 (root → L1 table)
    if !root_pt.entries[idx2].is_valid() {
        let (phys, l2) = alloc_pt(alloc);
        root_pt.entries[idx2] = PageTableEntry::new(paddr_to_ppn(phys), PTE_V);
        let (phys2, l1) = alloc_pt(alloc);
        l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(phys2), PTE_V);
        l1.entries[idx0] = PageTableEntry::new(paddr_to_ppn(paddr), rf);
    } else {
        let l2 = pt_at_mut(root_pt.entries[idx2].ppn());
        if !l2.entries[idx1].is_valid() {
            let (phys, l1) = alloc_pt(alloc);
            l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(phys), PTE_V);
            l1.entries[idx0] = PageTableEntry::new(paddr_to_ppn(paddr), rf);
        } else {
            let l1 = pt_at_mut(l2.entries[idx1].ppn());
            l1.entries[idx0] = PageTableEntry::new(paddr_to_ppn(paddr), rf);
        }
    }
}

pub fn map_2m(
    root: u64,
    alloc: &mut BitmapAllocator,
    vaddr: u64,
    paddr: u64,
    flags: PageFlags,
) {
    let root_pt = unsafe { &mut *(root as *mut PageTable) };
    let rf = page_flags_to_riscv(flags);

    let idx2 = vpn_index(vaddr, 2);
    let idx1 = vpn_index(vaddr, 1);

    if !root_pt.entries[idx2].is_valid() {
        let (phys, l2) = alloc_pt(alloc);
        root_pt.entries[idx2] = PageTableEntry::new(paddr_to_ppn(phys), PTE_V);
        l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(paddr), rf);
    } else {
        let l2 = pt_at_mut(root_pt.entries[idx2].ppn());
        l2.entries[idx1] = PageTableEntry::new(paddr_to_ppn(paddr), rf);
    }
}

pub fn unmap_4k(root: u64, _alloc: &mut BitmapAllocator, vaddr: u64) -> bool {
    let root_pt = unsafe { &mut *(root as *mut PageTable) };
    let idx2 = vpn_index(vaddr, 2);
    let idx1 = vpn_index(vaddr, 1);
    let idx0 = vpn_index(vaddr, 0);

    if !root_pt.entries[idx2].is_valid() {
        return false;
    }
    let l2 = pt_at_mut(root_pt.entries[idx2].ppn());
    if !l2.entries[idx1].is_valid() {
        return false;
    }
    let l1 = pt_at_mut(l2.entries[idx1].ppn());
    if !l1.entries[idx0].is_valid() {
        return false;
    }

    l1.entries[idx0].clear();
    unsafe { asm!("sfence.vma"); }
    true
}

pub fn translate(root: u64, vaddr: u64) -> Option<u64> {
    let root_pt = unsafe { &*(root as *const PageTable) };
    let idx2 = vpn_index(vaddr, 2);
    let idx1 = vpn_index(vaddr, 1);
    let idx0 = vpn_index(vaddr, 0);

    if !root_pt.entries[idx2].is_valid() {
        return None;
    }
    let l2 = pt_at(root_pt.entries[idx2].ppn());
    if !l2.entries[idx1].is_valid() {
        return None;
    }

    let entry = l2.entries[idx1];
    // Check for 2 MiB megapage (leaf at L1 — both R, W, or X set).
    if entry.0 & (PTE_R | PTE_W | PTE_X) != 0 {
        let base = entry.ppn() << 12;
        let offset = vaddr & 0x1F_FFFF;
        return Some(base | offset);
    }

    let l1 = pt_at(l2.entries[idx1].ppn());
    if !l1.entries[idx0].is_valid() {
        return None;
    }

    let pte = l1.entries[idx0];
    let base = pte.ppn() << 12;
    let offset = vaddr & 0xFFF;
    Some(base | offset)
}

/// Switch to the given root table (physical address of L2).
///
/// # Safety
/// The caller must ensure the new page table maps the current instruction
/// stream and stack.
pub unsafe fn activate(root: u64) {
    let satp = SATP_MODE_SV39 | paddr_to_ppn(root);
    unsafe { asm!("csrw satp, {}", in(reg) satp); }
    unsafe { asm!("sfence.vma"); }
}
