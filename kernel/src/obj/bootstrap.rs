//! C5 — Boot-domain bootstrap (§5). Arch-neutral.
//!
//! Called once from `Kernel::init()` after the service container exists and
//! before SMP bring-up. Creates the Boot domain, mints the primitive family
//! roots (P1 `StubNode`s; hooks arrive in P3), endows the real service
//! providers (DMA / PCI-config / serial) as capabilities reachable only
//! through the boot table (§5.4, §7.7.1), and keeps the boot domain so that
//! `init()` can self-revoke the mint guard as its last statement.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;

use spin::Once;

use super::adapters;
use super::cap_handle::{CapHandle, CapId, HandleState};
use super::domain::{self, Domain};
use super::mint::{self, PrincipalContext};
use super::registry;
use super::rights::{CapRights, ContractRights, Rights};
use super::store::{object_store, StoreNode};
use super::table;
use super::{invoke, Args, Obj, Value};

/// The provider capabilities handed to the Boot domain. Later phases (C6/C7)
/// recover these `CapId`s to `invoke` through `boot_domain().table.resolve(...)`.
#[derive(Clone, Copy)]
pub struct BootEndowment {
    pub dma: CapId,
    pub pci_cfg: CapId,
    pub serial: CapId,
    /// The contract-registry capability (§7.8): the boot domain is the first
    /// domain endowed to consult "what does `dma:alloc` promise?".
    pub registry: CapId,
}

/// Rights held by the Boot domain over each primitive family root (§5.4).
const PRIM_RIGHTS: Rights = Rights::INVOKE.or(Rights::QUERY).or(Rights::TRAVERSE);

static BOOT_DOMAIN: Once<&'static Domain> = Once::new();
static BOOT_ENDOWMENT: Once<BootEndowment> = Once::new();

/// Build the Boot domain, mint the primitive family roots, and endow the
/// real service providers as capabilities. Returns a reference to the boot
/// domain (said to become the BSP's current domain).
pub fn bootstrap() -> &'static Domain {
    let boot: &'static Domain = Box::leak(Box::new(Domain::new(0)));
    domain::set_current_domain(boot);

    let principal = PrincipalContext;

    // §5.1 primitive service family roots — P1 placeholders whose concrete
    // hook bodies arrive in P3. The Boot domain holds the roots.
    for kind in [
        "prim:physmem",
        "prim:heap",
        "prim:addrspace",
        "prim:cpu",
        "prim:irq",
    ] {
        let cap = mint::mint(&principal, kind, PRIM_RIGHTS).expect("mint primitive family root");
        boot.table.insert(cap);
    }

    // §5.1 / §7.7.1 — endow the real service providers; reachable in this
    // phase, but only through the boot table.
    let dma_id = boot.table.insert(CapHandle {
        id: CapId(0),
        node: adapters::dma_node(),
        rights: CapRights::new(Rights::INVOKE, ContractRights::empty()),
        state: HandleState::Live,
    });
    let pci_cfg_id = boot.table.insert(CapHandle {
        id: CapId(0),
        node: adapters::pci_cfg_node(),
        rights: CapRights::new(Rights::INVOKE, ContractRights::empty()),
        state: HandleState::Live,
    });
    let serial_id = boot.table.insert(CapHandle {
        id: CapId(0),
        node: adapters::serial_node(),
        rights: CapRights::new(Rights::INVOKE, ContractRights::empty()),
        state: HandleState::Live,
    });

    // §7.8 — the contract registry is a node, endowed like any provider. Build
    // the registry node, insert it as a capability, and seed the real provider
    // contracts THROUGH that capability (the INVOKE-gated `register` hook), so
    // the registry carries their definitions before separation proves it.
    let registry_id = boot.table.insert(CapHandle {
        id: CapId(0),
        node: registry::registry_node(),
        rights: CapRights::new(Rights::INVOKE.or(Rights::QUERY), ContractRights::empty()),
        state: HandleState::Live,
    });
    for def in [
        adapters::dma_contract_def(),
        adapters::pci_contract_def(),
        adapters::serial_contract_def(),
    ] {
        let args = Args { vals: vec![Value::Str(def.name)] };
        invoke(
            &boot.table,
            registry_id,
            registry::REGISTRY_CONTRACT,
            registry::REGISTRY_REGISTER,
            &args,
        )
        .expect("bootstrap: register provider contract");
    }

    // §7.3 / §7.8 — register every stable-id infra/adapter node in the
    // ObjectStore so the `kerneldump graph` census reflects the P2 model, not
    // just the minted primitive roots. `register_with_id` keeps the store weak
    // (records hold no node reference) while making the deterministic ids
    // visible to the projection tool. All are boot-era seeds: parent = none.
    register_seed_node(table::table_node(&boot.table).as_ref());
    register_seed_node(&StoreNode);
    register_seed_node(&registry::RegistryNode);
    register_seed_node(adapters::dma_node().as_ref());
    register_seed_node(adapters::pci_cfg_node().as_ref());
    register_seed_node(adapters::serial_node().as_ref());

    BOOT_DOMAIN.call_once(|| boot);
    BOOT_ENDOWMENT.call_once(|| BootEndowment {
        dma: dma_id,
        pci_cfg: pci_cfg_id,
        serial: serial_id,
        registry: registry_id,
    });

    // C8 — the first driver domain (§6.2): a second, disjoint table endowed
    // only with dma + pci_cfg. Created eagerly alongside the boot domain so the
    // separation property holds from the first commit and `separation.rs::run()`
    // can prove it before anything else runs.
    super::driver::create();
    boot
}

/// The Boot domain, once bootstrapped (§6.2).
pub fn boot_domain() -> &'static Domain {
    *BOOT_DOMAIN.get().expect("boot domain not bootstrapped")
}

/// The Boot domain's endowment. C6/C7 use `boot_endowment().dma` /
/// `.pci_cfg` / `.serial` to `invoke` the real providers.
pub fn boot_endowment() -> &'static BootEndowment {
    BOOT_ENDOWMENT.get().expect("boot endowment not bootstrapped")
}

/// Register a stable-id boot-era seed node in the ObjectStore under the
/// identity its `Obj` impl reports (§7.3, §7.8). Read-only with respect to the
/// node: the store keeps a weak record only, so reach/lifetime is unaffected.
fn register_seed_node(node: &dyn Obj) {
    object_store().register_with_id(node.obj_id(), node.kind(), None);
}
