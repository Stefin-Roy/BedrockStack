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