//! C8 — the first driver domain (§6.2, §8.14). Arch-neutral.
//!
//! Created eagerly in `bootstrap()` alongside the boot domain so that both
//! disjoint domains exist from the first commit. The C8 separation property
//! is structural — disjoint capability tables plus disjoint address spaces —
//! not asserted by a dedicated boot-time proof (the proof module was removed
//! in 6adbc4e; the `selftest`-gated kerneldump census is the only boot-time
//! verification). The driver
//! domain holds its OWN capability table (disjoint from the boot domain's) and
//! is endowed with ONLY its controllers' provider caps — dma + pci_cfg, plus
//! the physical nodes the device sweep works over: physmem + addrspace + irq.
//! It holds no heap, no serial, and no other primitive family roots, so "the
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
/// physical nodes the sweep works over: physmem + addrspace + irq. No heap, no
/// serial, no cpu.
#[derive(Clone, Copy)]
pub struct DriverEndowment {
    pub dma: CapId,
    pub pci_cfg: CapId,
    pub physmem: CapId,
    pub addrspace: CapId,
    pub irq: CapId,
}

static DRIVER_DOMAIN: Once<&'static Domain> = Once::new();
static DRIVER_ENDOWMENT: Once<DriverEndowment> = Once::new();

/// Contract-right mask held by the driver domain over its controllers'
/// providers and the physical nodes the device sweep works over: the full
/// READ|WRITE|CALL set, so every per-hook requirement (`hook_contract_right`)
/// of the dma / pci_cfg / physmem / addrspace / irq nodes passes from creation.
const DRIVER_CONTRACT: ContractRights = ContractRights::READ.or(ContractRights::WRITE).or(ContractRights::CALL);

/// Create the first driver domain (§6.2): a second, disjoint table endowed
/// only with dma + pci_cfg + physmem + addrspace + irq. Called once from
/// `bootstrap()`, so both domains coexist from `init()`. The separation is
/// structural: disjoint capability tables and disjoint address spaces.
///
/// Paged isolation (§8.14): the driver domain owns its own address space
/// — a fresh root cloned from the kernel's higher half (`parent_root`), empty
/// in the low half — so its memory is structurally unreachable from the boot
/// domain's page tables and vice versa.
pub fn create(parent_root: u64) {
    let driver: &'static Domain = Domain::with_addrspace(
        1,
        parent_root,
        alloc::sync::Arc::new(crate::ns::namespace::Namespace::new()),
    );

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
    // too (INVOKE-only, no heap — the heap stays a boot-domain secret,
    // structurally unreachable from the driver table since no heap cap is
    // endowed).
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
    // The device sweep registers interrupt handlers through the irq family
    // root; endow it too (INVOKE-only, full contract mask so both the
    // CALL-gated register/ack and the WRITE-gated unregister/set_enabled
    // hooks pass).
    let irq_id = driver.table.insert(CapHandle {
        id: CapId(0),
        node: nodes::irq_root_node(),
        rights: CapRights::new(Rights::INVOKE, DRIVER_CONTRACT),
        state: HandleState::Live,
    });

    DRIVER_DOMAIN.call_once(|| driver);
    DRIVER_ENDOWMENT.call_once(|| DriverEndowment {
        dma: dma_id,
        pci_cfg: pci_cfg_id,
        physmem: physmem_id,
        addrspace: addrspace_id,
        irq: irq_id,
    });
}

/// The first driver domain, once created (§6.2).
pub fn driver_domain() -> &'static Domain {
    *DRIVER_DOMAIN.get().expect("driver domain not created")
}

/// The driver domain's endowment (dma + pci_cfg + physmem + addrspace + irq,
/// and nothing else).
pub fn driver_endowment() -> &'static DriverEndowment {
    DRIVER_ENDOWMENT.get().expect("driver endowment not created")
}
