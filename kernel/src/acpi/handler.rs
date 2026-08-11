//! `aml` crate `Handler` implementation for BedrockOS (x86_64).
//!
//! SystemMemory op-region addresses are physical addresses, but the kernel's
//! page tables are higher-half only, so every access is mapped on demand
//! through the ACPI VMM with a small page cache (the VMM region is a bump
//! allocator, so repeated op-region accesses must not re-map every time).
//! SystemIO and PCI accesses route through the existing port I/O and ECAM
//! paths. All methods are infallible by trait contract; unmapped ECAM regions
//! return all-ones (empty config space) rather than failing.

use spin::Mutex;
use ::aml::Handler;

use crate::mm::vmm::PageFlags;

const CACHE_SLOTS: usize = 8;

struct MmioCache {
    entries: [Option<(u64, u64)>; CACHE_SLOTS],
    next: usize,
}

static MMIO_CACHE: Mutex<MmioCache> = Mutex::new(MmioCache {
    entries: [None; CACHE_SLOTS],
    next: 0,
});

/// Map the physical page containing `phys` and return the virtual address of
/// `phys` itself.
fn mmio_addr(phys: u64) -> u64 {
    let page = phys & !0xFFF;
    let mut cache = MMIO_CACHE.lock();
    for &slot in cache.entries.iter() {
        if let Some((p, v)) = slot {
            if p == page {
                return v + (phys & 0xFFF);
            }
        }
    }
    let vaddr = crate::acpi::map_device_mmio(
        page,
        0x1000,
        PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
    );
    let idx = cache.next;
    cache.entries[idx] = Some((page, vaddr));
    cache.next = (idx + 1) % CACHE_SLOTS;
    vaddr + (phys & 0xFFF)
}

/// The AML handler for BedrockOS.
#[derive(Clone, Copy)]
pub struct AmlHandler;

impl Handler for AmlHandler {
    fn read_u8(&self, address: usize) -> u8 {
        unsafe { (mmio_addr(address as u64) as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { (mmio_addr(address as u64) as *const u16).read_volatile() }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { (mmio_addr(address as u64) as *const u32).read_volatile() }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { (mmio_addr(address as u64) as *const u64).read_volatile() }
    }

    fn write_u8(&mut self, address: usize, value: u8) {
        unsafe { (mmio_addr(address as u64) as *mut u8).write_volatile(value) }
    }

    fn write_u16(&mut self, address: usize, value: u16) {
        unsafe { (mmio_addr(address as u64) as *mut u16).write_volatile(value) }
    }

    fn write_u32(&mut self, address: usize, value: u32) {
        unsafe { (mmio_addr(address as u64) as *mut u32).write_volatile(value) }
    }

    fn write_u64(&mut self, address: usize, value: u64) {
        unsafe { (mmio_addr(address as u64) as *mut u64).write_volatile(value) }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        crate::acpi::gas::port_in(port, 8).unwrap_or(0) as u8
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        crate::acpi::gas::port_in(port, 16).unwrap_or(0) as u16
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        crate::acpi::gas::port_in(port, 32).unwrap_or(0)
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        let _ = crate::acpi::gas::port_out(port, value as u32, 8);
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        let _ = crate::acpi::gas::port_out(port, value as u32, 16);
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        let _ = crate::acpi::gas::port_out(port, value, 32);
    }

    fn read_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
        crate::pci::ecam::read_u8(segment, bus, device, function, offset)
    }

    fn read_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
        crate::pci::ecam::read_u16(segment, bus, device, function, offset)
    }

    fn read_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        crate::pci::ecam::read_u32(segment, bus, device, function, offset)
    }

    fn write_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u8) {
        crate::pci::ecam::write_u8(segment, bus, device, function, offset, value)
    }

    fn write_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u16) {
        crate::pci::ecam::write_u16(segment, bus, device, function, offset, value)
    }

    fn write_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u32) {
        crate::pci::ecam::write_u32(segment, bus, device, function, offset, value)
    }

    fn handle_fatal_error(&self, fatal_type: u8, fatal_code: u32, fatal_arg: u64) {
        log::error!(
            "ACPI: AML DefFatal -- type={}, code=0x{:x}, arg=0x{:x}",
            fatal_type,
            fatal_code,
            fatal_arg
        );
    }
}