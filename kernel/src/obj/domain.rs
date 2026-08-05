extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use spin::Mutex;
use spin::Once;

use super::table::CapabilityTable;
use crate::mm::vmm::Vmm;

/// An independent principal-in-the-small: an execution context holding its own
/// capability table (§6).
///
/// Paged isolation (§8.14): a non-kernel domain also owns its own address
/// space — a page-table root cloned from the kernel's higher half, empty in the
/// low half — so two domains cannot reach each other's memory by position. The
/// kernel (boot) domain is `addrspace = None` + `is_kernel = true`; it *is* the
/// kernel's root and never switches CR3 away from it.
pub struct Domain {
    pub table: CapabilityTable,
    pub id: u32,
    pub addrspace: Option<Vmm>,
    pub is_kernel: bool,
}

impl Domain {
    pub const fn new(id: u32) -> Self {
        Domain {
            table: CapabilityTable::new(),
            id,
            addrspace: None,
            is_kernel: false,
        }
    }

    /// Build a non-kernel domain with its own address space: a fresh root that
    /// inherits the kernel's higher-half mappings from `parent_root` (the
    /// active kernel root) and starts with an empty low half (§8.14).
    ///
    /// The clone deliberately SHARES the kernel root's PML4/PDPT subtrees (see
    /// `mm::vmm::clone_high_half`) rather than re-mapping the device windows
    /// per-domain: the device sweep maps ECAM/DMA/MMIO lazily into the kernel
    /// root after the clone, and only the shared subtrees keep those mappings
    /// visible under this domain's CR3.  The kernel root outlives every clone.
    ///
    /// # Panics
    /// - On fatal memory shortage while cloning the page tables.
    pub fn with_addrspace(id: u32, parent_root: u64) -> &'static Domain {
        let alloc = crate::mm::heap::get_phys_allocator_mut();
        let root = crate::mm::vmm::clone_high_half(alloc, parent_root);
        let d: &'static Domain = Box::leak(Box::new(Domain {
            table: CapabilityTable::new(),
            id,
            addrspace: Some(Vmm::from_root(root)),
            is_kernel: false,
        }));
        d
    }

    /// Bind a non-empty address space (used for the kernel domain, so returning
    /// to it re-activates the kernel root).
    pub fn set_kernel_addrspace(&mut self, root: u64) {
        self.addrspace = Some(Vmm::from_root(root));
        self.is_kernel = true;
    }

    /// This domain's page-table root, if it owns a distinct address space.
    pub fn page_root(&self) -> Option<u64> {
        self.addrspace.as_ref().map(|v| v.root())
    }
}

/// Set the current domain on this CPU's per-CPU slot (§6.3) and, if the domain
/// owns a distinct address space, switch CR3/SATP to it so the domain actually
/// executes in its own page tables (§8.14).
pub fn set_current_domain(d: &'static Domain) {
    let pc = crate::smp::current_per_cpu();
    pc.current_domain = d as *const Domain;
    if let Some(root) = d.page_root() {
        // Every domain table carries the kernel higher-half (via the clone), so
        // the IDT, per-CPU GS/GDT/TSS data and the current stack stay reachable
        // across the switch (§6.4).
        unsafe { crate::mm::vmm::activate(root) };
    }
}

/// Return the current domain for this CPU, if BSP init has run (§6.3).
pub fn current_domain() -> Option<&'static Domain> {
    let pc = crate::smp::try_current_per_cpu()?;
    let p = pc.current_domain;
    if p.is_null() {
        None
    } else {
        // SAFETY: `current_domain` is only ever written via `set_current_domain`
        // with a `&'static Domain`, so a non-null pointer is a live reference.
        Some(unsafe { &*p })
    }
}

/// The domain registry: every domain ever created, for the projection tool's
/// `held-by` report and the leak detector's reachability roots (§7.13, §8.7).
/// The store stays weak; this is a set of *tables*, not a namespace.
static DOMAINS: Once<Mutex<Vec<&'static Domain>>> = Once::new();

fn domain_list() -> &'static Mutex<Vec<&'static Domain>> {
    DOMAINS.call_once(|| Mutex::new(Vec::new()))
}

/// Register a domain so the projection tool can see what it holds (§7.13).
/// Called once per domain at creation (boot, driver, revocation gate).
pub fn register_domain(d: &'static Domain) {
    domain_list().lock().push(d);
}

/// Snapshot of every registered domain, in creation order.
pub fn all_domains() -> Vec<&'static Domain> {
    domain_list().lock().clone()
}