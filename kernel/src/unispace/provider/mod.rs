pub mod acpi;
pub mod arch;
pub mod block;
pub mod dev;
pub mod driver;
pub mod input;
#[cfg(target_arch = "x86_64")]
pub mod kdump;
#[cfg(target_arch = "x86_64")]
pub mod kernel;
pub mod mm;
pub mod pci;
pub mod platform;
#[cfg(target_arch = "x86_64")]
pub mod proc;
pub mod ps2;
pub mod random;
pub mod smp_sched;
pub mod sys;
pub mod usb;
pub mod vfs;

use super::UnispaceError;

/// Register all unispace providers.  Called once after VFS init.
pub fn register_all() -> Result<(), UnispaceError> {
    dev::register()?;
    vfs::register()?;
    sys::register()?;
    driver::register()?;
    input::register()?;
    #[cfg(target_arch = "x86_64")]
    kernel::register()?;
    #[cfg(target_arch = "x86_64")]
    proc::register()?;
    // New providers — order matters: need /sys and /kernel dirs.
    mm::register()?;
    arch::register()?;
    smp_sched::register()?;
    acpi::register()?;
    pci::register()?;
    platform::register()?;
    #[cfg(target_arch = "x86_64")]
    kdump::register()?;
    block::register()?;
    ps2::register()?;
    usb::register()?;
    Ok(())
}
