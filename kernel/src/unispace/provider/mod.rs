pub mod sys;
pub mod vfs;

use super::UnispaceError;

/// Register all unispace providers.  Called once after VFS init.
pub fn register_all() -> Result<(), UnispaceError> {
    vfs::register()?;
    sys::register()?;
    Ok(())
}
