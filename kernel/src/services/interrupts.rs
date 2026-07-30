use super::capability::Capability;

pub trait InterruptManager: Capability {
    fn register_handler(&self, vector: u8, handler: fn());
    fn unregister_handler(&self, vector: u8);
    fn enable(&self, vector: u8);
    fn disable(&self, vector: u8);
    fn eoi(&self);
}
