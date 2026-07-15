pub mod fat32_test;
pub mod registry;
pub mod vfs_test;

use framebuffer::Framebuffer;

/// Module trait for loadable kernel modules.
///
/// # Invariants
/// - INV-MT-01: name() returns valid UTF-8 for 'static lifetime
/// - INV-MT-02: init() mutates only the provided display reference
pub trait Module: Sync {
    /// Get the module name.
    fn name(&self) -> &str;

    /// Get the module version.
    fn version(&self) -> &str;

    /// Initialize the module.
    ///
    /// Called once during kernel startup.
    /// Returns Ok(()) on success, Err(message) on failure.
    fn init(&self, display: &mut Framebuffer) -> Result<(), &'static str>;
}
