//! C8 — the first driver domain (§6.2, §8.14). Arch-neutral.
//!
//! Created eagerly in `bootstrap()` alongside the boot domain so that both
//! disjoint domains exist from the first commit and `separation.rs::run()`
//! (called in `Kernel::init()`) can prove the separation property. The driver
//! domain holds its OWN capability table (disjoint from the boot domain's) and
//! is endowed with ONLY its controllers' provider caps — dma + pci_cfg, plus
//! the physical nodes the device sweep works over: physmem + addrspace. It
//! holds no heap, no serial, and no other primitive family roots, so "the
//! kernel cannot silently reach the driver's addresses by position" (§6.2): a
//! cap the driver table never received resolves to `NoSuchCap` / `Denied`.

extern crate alloc;

use spin::Once;

use super::adapters;
use super::cap_handle::{CapHandle, CapId, HandleState};
use super::domain::Domain;
use super::nodes;
use super::rights::{CapRights, ContractRights, Rights};

/// The provider capabilities handed to the first driver domain (§6.2).
/// Exactly the device-sweep controllers' providers: dma + pci_cfg, plus the
/// physical nodes the sweep works over: physmem + addrspace. No heap, no
/// serial, no cpu, no irq.
#[derive(Clone, Copy)]
pub struct DriverEndowment {
    pub dma: CapId,
    pub pci_cfg: CapId,
    pub physmem: CapId,
    pub addrspace: CapId,
}

static DRIVER_DOMAIN: Once<&'static Domain> = Once::new();
static DRIVER_ENDOWMENT: Once<DriverEndowment> = Once::new();

/// Contract-right mask held by the driver domain over its controllers'
/// providers and the physical nodes the device sweep works over: the full
/// READ|WRITE|CALL set, so every per-hook requirement (`hook_contract_right`)
/// of the dma / pci_cfg / physmem / addrspace nodes passes from creation.
const DRIVER_CONTRACT: ContractRights = ContractRights::READ.or(ContractRights::WRITE).or(ContractRights::CALL);

/// Create the first driver domain (§6.2): a second, disjoint table endowed
/// only with dma + pci_cfg + physmem + addrspace. Called once from
/// `bootstrap()`, so both domains exist during `init()` and the C8 separation
/// proof runs before SMP.
///
/// Paged isolation (§8.14): the driver domain owns its own address space
/// — a fresh root cloned from the kernel's higher half (`parent_root`), empty
/// in the low half — so its memory is structurally unreachable from the boot
/// domain's page tables and vice versa.
pub fn create(parent_root: u64) {
    let driver: &'static Domain = Domain::with_addrspace(1, parent_root);

    // Endow the driver domain by constructing its CapHandles directly from the
    // same provider nodes the boot domain was endowed with (§5.4); only the
    // INVOKE right is granted, mirroring `bootstrap()`.
    let dma_id = driver.table.insert(CapHandle {
        id: CapId(0),
        node: adapters::dma_node(),
        rights: CapRights::new(Rights::INVOKE, DRIVER_CONTRACT),
        state: HandleState::Live,
    });
    let pci_cfg_id = driver.table.insert(CapHandle {
        id: CapId(0),
        node: adapters::pci_cfg_node(),
        rights: CapRights::new(Rights::INVOKE, DRIVER_CONTRACT),
        state: HandleState::Live,
    });
    // The device sweep allocates frames and maps them; endow those nodes
    // too (INVOKE-only, no heap — the heap stays a boot-domain secret, which
    // separation.rs proves negatively).
    let physmem_id = driver.table.insert(CapHandle {
        id: CapId(0),
        node: nodes::phys_mem_node(),
        rights: CapRights::new(Rights::INVOKE, DRIVER_CONTRACT),
        state: HandleState::Live,
    });
    let addrspace_id = driver.table.insert(CapHandle {
        id: CapId(0),
        node: nodes::addr_space_node(),
        rights: CapRights::new(Rights::INVOKE, DRIVER_CONTRACT),
        state: HandleState::Live,
    });

    DRIVER_DOMAIN.call_once(|| driver);
    DRIVER_ENDOWMENT.call_once(|| DriverEndowment {
        dma: dma_id,
        pci_cfg: pci_cfg_id,
        physmem: physmem_id,
        addrspace: addrspace_id,
    });
}

/// The first driver domain, once created (§6.2).
pub fn driver_domain() -> &'static Domain {
    *DRIVER_DOMAIN.get().expect("driver domain not created")
}

/// The driver domain's endowment (dma + pci_cfg + physmem + addrspace, and
/// nothing else).
pub fn driver_endowment() -> &'static DriverEndowment {
    DRIVER_ENDOWMENT.get().expect("driver endowment not created")
}
