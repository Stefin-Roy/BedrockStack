use super::capability::Capability;

pub trait Clockevent: Capability {
    fn set_deadline(&self, deadline_ns: u64);
    fn stop(&self);
}
