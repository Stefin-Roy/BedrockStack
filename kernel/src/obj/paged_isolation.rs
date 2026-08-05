//! Paged domain isolation proof (§8.14).
//!
//! Boot-time and arch-neutral. Proves the driver domain's own page tables are
//! structurally disjoint from the boot domain's in the low half, share the
//! kernel higher half under both CR3/SATP roots, and that table mutation only
//! ever flows through the capability API.

use super::adapters::{self, DMA_ALLOC_PAGE};
use super::bootstrap::boot_domain;
use super::cap_handle::CapId;
use super::driver::{driver_domain, driver_endowment};
use super::{invoke, Args, ObjError};
use crate::drivers::serial::SerialPort;
use crate::mm::heap::get_phys_allocator_mut;
use crate::mm::layout::{to_physmap, KERNEL_VMA_BASE};
use crate::mm::vmm::{PageFlags, Vmm};

/// A 4K-aligned virtual address in the low half of both address spaces
/// (bit 47 clear on x86_64, bit 38 clear in Sv39), inside no existing window.
const CANARY_VA: u64 = 0x0000_8000_0000;

pub fn run() {
    let boot = boot_domain();
    let drv = driver_domain();
    let dend = driver_endowment();

    let boot_root_phys = boot.page_root().expect("paged: boot domain has no addrspace");
    let drv_root_phys = drv.page_root().expect("paged: driver domain has no addrspace");
    let boot_vmm = Vmm::from_root(boot_root_phys);
    let mut drv_vmm = Vmm::from_root(drv_root_phys);

    // (1) Disjoint low halves.
    assert!(
        CANARY_VA < KERNEL_VMA_BASE,
        "paged: canary VA must live below the kernel higher half"
    );
    assert_ne!(
        drv_root_phys,
        boot_root_phys,
        "paged: boot and driver domains share a root frame"
    );

    let mut alloc = get_phys_allocator_mut();
    let canary_phys = alloc.alloc().expect("paged: OOM for canary frame");
    assert_ne!(
        canary_phys,
        CANARY_VA,
        "paged: canary frame collides with the canary VA (identity-map alias)"
    );

    // The driver root's low half starts empty: the clone carries no low-half
    // mappings, so the canary VA must be unmapped before we touch it.
    assert!(
        drv_vmm.translate(CANARY_VA).is_none(),
        "paged FAIL: driver root low half not empty before canary map"
    );
    drv_vmm.map_4k(
        &mut alloc,
        CANARY_VA,
        canary_phys,
        PageFlags::READ | PageFlags::WRITE,
    );

    assert_eq!(
        drv_vmm.translate(CANARY_VA),
        Some(canary_phys),
        "paged FAIL: driver root cannot translate the mapped canary VA"
    );
    assert_ne!(
        boot_vmm.translate(CANARY_VA),
        Some(canary_phys),
        "paged FAIL: boot root reaches the driver's low-half page by position"
    );
    SerialPort::puts("[obj] paged ok: disjoint low halves va=0x");
    SerialPort::put_hex(CANARY_VA);
    SerialPort::puts(" phys=0x");
    SerialPort::put_hex(canary_phys);
    SerialPort::puts("\n");

    // (2) Shared kernel half: the physmap alias of the frame resolves to the
    // same physical frame under both roots, so the kernel (IDT, per-CPU data,
    // stacks, capability tables) stays reachable from either domain.
    let alias = to_physmap(canary_phys);
    assert_eq!(
        boot_vmm.translate(alias),
        Some(canary_phys),
        "paged FAIL: boot root lost the kernel higher-half alias"
    );
    assert_eq!(
        drv_vmm.translate(alias),
        Some(canary_phys),
        "paged FAIL: driver root lost the kernel higher-half alias"
    );
    SerialPort::puts("[obj] paged ok: shared kernel half phys=0x");
    SerialPort::put_hex(canary_phys);
    SerialPort::puts(" alias=0x");
    SerialPort::put_hex(alias);
    SerialPort::puts("\n");

    // (3) Capability-mediated mutation only: a real allocation succeeds through
    // the endowed DMA cap (invoke, not just resolve), an unendowed id is
    // refused, and the two domains hold structurally disjoint root frames.
    match invoke(&drv.table, dend.dma, adapters::DMA_CONTRACT, DMA_ALLOC_PAGE, &Args::none()) {
        Ok(_) => SerialPort::puts("[obj] paged ok: endowed dma mutates through cap only\n"),
        Err(e) => panic!("paged FAIL: endowed dma invoke refused: {:?}", e),
    }

    match drv.table.resolve(CapId(u32::MAX as u64), adapters::DMA_CONTRACT, DMA_ALLOC_PAGE) {
        Err(ObjError::NoSuchCap) => {
            SerialPort::puts("[obj] paged ok: unendowed id refused\n")
        }
        Ok(_) => panic!("paged FAIL: unendowed id resolved"),
        Err(e) => panic!("paged FAIL: unendowed id -> {:?}", e),
    }

    SerialPort::puts("[obj] paged ok: disjoint root frames boot=0x");
    SerialPort::put_hex(boot_root_phys);
    SerialPort::puts(" driver=0x");
    SerialPort::put_hex(drv_root_phys);
    SerialPort::puts("\n");

    SerialPort::puts("[obj] paged isolation: OK\n");
}
