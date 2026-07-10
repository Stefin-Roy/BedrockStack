//! Driver trait definitions.
//!
//! This module defines the contracts for hardware drivers.
//! No implementations exist yet - this is honest.
//!
//! Future drivers must implement these traits.
//! The kernel core depends only on these trait definitions.

use crate::display::framebuffer::Framebuffer;

/// Driver trait for hardware drivers.
///
/// # Invariants
/// - INV-DR-01: init() must not panic
/// - INV-DR-02: shutdown() must be safe to call multiple times
/// - INV-DR-03: name() must return a valid identifier string
pub trait Driver {
    /// Initialize the driver.
    ///
    /// Called once during kernel startup.
    /// Returns Ok(()) on success, Err(message) on failure.
    fn init(&mut self, display: &mut Framebuffer) -> Result<(), &'static str>;

    /// Get the driver name.
    fn name(&self) -> &str;

    /// Shutdown the driver.
    ///
    /// Must be safe to call even if driver is already shut down.
    fn shutdown(&mut self);
}
