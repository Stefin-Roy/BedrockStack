
pub trait PlatformControl: Send + Sync {
    fn shutdown(&self) -> !;
    fn reset(&self) -> !;
    fn halt(&self);
    fn disable_interrupts(&self);
    fn enable_interrupts(&self);
    fn are_interrupts_enabled(&self) -> bool;
}
