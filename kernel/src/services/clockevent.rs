
pub trait Clockevent: Send + Sync {
    fn set_deadline(&self, deadline_ns: u64);
    fn stop(&self);
}
