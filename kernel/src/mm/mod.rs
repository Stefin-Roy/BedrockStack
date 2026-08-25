pub mod framecnt;
pub mod heap;
pub mod layout;
pub mod phys_alloc;
pub mod usermem;
pub mod vmm;

#[cfg(target_arch = "x86_64")]
pub mod fault;

/// Kernel-internal physmap access. Only `mm`/`arch`/`smp` may use this.
pub use layout::{init_physmap, phys_offset, physmap_end, to_physmap};
