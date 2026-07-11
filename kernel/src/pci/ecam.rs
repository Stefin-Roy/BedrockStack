use alloc::vec::Vec;
use spin::Mutex;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm, KERNEL_VMA_BASE};
use acpi::platform::pci::PciConfigRegions;

const PCI_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x10000000 - 0x20000000;

struct PciVmmState {
    root: u64,
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
}

unsafe impl Send for PciVmmState {}
unsafe impl Sync for PciVmmState {}

static PCI_VMM: Mutex<Option<PciVmmState>> = Mutex::new(None);

pub fn init_vmm(root: u64, alloc: *mut BitmapAllocator) {
    *PCI_VMM.lock() = Some(PciVmmState {
        root,
        alloc,
        next_vaddr: PCI_VADDR_BASE,
    });
}

fn map_ecam(paddr: u64, size: u64) -> u64 {
    let mut guard = PCI_VMM.lock();
    let state = guard.as_mut().expect("PCI VMM not initialized");
    let vaddr = state.next_vaddr - size;
    state.next_vaddr = vaddr;
    let mut vmm = Vmm::from_root(state.root);
    let alloc = unsafe { &mut *state.alloc };
    vmm.map(alloc, vaddr, paddr, size, PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE);
    vaddr
}

struct MappedRegion {
    segment: u16,
    bus_start: u8,
    bus_end: u8,
    virt_base: u64,
}

impl MappedRegion {
    fn contains(&self, segment: u16, bus: u8) -> bool {
        self.segment == segment && bus >= self.bus_start && bus <= self.bus_end
    }

    fn virt_addr(&self, bus: u8, device: u8, function: u8, offset: u16) -> *mut u8 {
        let a = self.virt_base
            | ((bus as u64 - self.bus_start as u64) << 20)
            | ((device as u64) << 15)
            | ((function as u64) << 12)
            | (offset as u64);
        a as *mut u8
    }
}

static MAPPED: Mutex<Option<Vec<MappedRegion>>> = Mutex::new(None);

pub fn map_all(regions: &PciConfigRegions) {
    let mut mapped = Vec::new();
    for entry in &regions.regions {
        let num_buses = (entry.bus_number_end - entry.bus_number_start + 1) as u64;
        let size = num_buses << 20;
        let vaddr = map_ecam(entry.base_address, size);
        mapped.push(MappedRegion {
            segment: entry.pci_segment_group,
            bus_start: entry.bus_number_start,
            bus_end: entry.bus_number_end,
            virt_base: vaddr,
        });
    }
    *MAPPED.lock() = Some(mapped);
}

fn find_region(segment: u16, bus: u8) -> Option<&'static MappedRegion> {
    let guard = MAPPED.lock();
    let mapped = guard.as_ref()?;
    for r in mapped.iter() {
        if r.contains(segment, bus) {
            return Some(unsafe { &*(r as *const MappedRegion) });
        }
    }
    None
}

pub fn read_u32(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return 0xFFFF_FFFF,
    };
    unsafe { (r.virt_addr(bus, device, function, offset) as *const u32).read_volatile() }
}

pub fn read_u16(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return 0xFFFF,
    };
    unsafe { (r.virt_addr(bus, device, function, offset) as *const u16).read_volatile() }
}

pub fn read_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return 0xFF,
    };
    unsafe { (r.virt_addr(bus, device, function, offset) as *const u8).read_volatile() }
}

pub fn write_u32(segment: u16, bus: u8, device: u8, function: u8, offset: u16, val: u32) {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return,
    };
    unsafe { (r.virt_addr(bus, device, function, offset) as *mut u32).write_volatile(val); }
}

pub fn write_u16(segment: u16, bus: u8, device: u8, function: u8, offset: u16, val: u16) {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return,
    };
    unsafe { (r.virt_addr(bus, device, function, offset) as *mut u16).write_volatile(val); }
}

pub fn write_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16, val: u8) {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return,
    };
    unsafe { (r.virt_addr(bus, device, function, offset) as *mut u8).write_volatile(val); }
}

/// Read entire 256-byte config header into a buffer.
pub fn read_header(segment: u16, bus: u8, device: u8, function: u8, buf: &mut [u8; 256]) {
    let r = match find_region(segment, bus) {
        Some(r) => r,
        None => return,
    };
    let base = r.virt_addr(bus, device, function, 0);
    unsafe {
        core::ptr::copy_nonoverlapping(base, buf.as_mut_ptr(), 256);
    }
}
