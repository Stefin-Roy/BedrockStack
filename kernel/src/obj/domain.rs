extern crate alloc;

use alloc::vec::Vec;
use spin::Mutex;
use spin::Once;

use super::table::CapabilityTable;

/// An independent principal-in-the-small: an execution context holding its own
/// capability table (§6).
pub struct Domain {
    pub table: CapabilityTable,
    pub id: u32,
}

impl Domain {
    pub const fn new(id: u32) -> Self {
        Domain {
            table: CapabilityTable::new(),
            id,
        }
    }
}

/// Set the current domain on this CPU's per-CPU slot (§6.3).
pub fn set_current_domain(d: &'static Domain) {
    let pc = crate::smp::current_per_cpu();
    pc.current_domain = d as *const Domain;
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
/// Called once per domain at creation (boot, driver, P5 gate).
pub fn register_domain(d: &'static Domain) {
    domain_list().lock().push(d);
}

/// Snapshot of every registered domain, in creation order.
pub fn all_domains() -> Vec<&'static Domain> {
    domain_list().lock().clone()
}