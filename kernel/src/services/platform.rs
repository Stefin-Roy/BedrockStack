use super::capability::Capability;

pub trait PlatformControl: Capability {
    fn shutdown(&self) -> !;
    fn reset(&self) -> !;
    fn halt(&self);
    fn disable_interrupts(&self);
    fn enable_interrupts(&self);
    fn are_interrupts_enabled(&self) -> bool;
}
