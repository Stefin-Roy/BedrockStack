pub mod dev;
pub mod driver;
pub mod input;
#[cfg(target_arch = "x86_64")]
pub mod kernel;
#[cfg(target_arch = "x86_64")]
pub mod proc;
pub mod sys;
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
    Ok(())
}
