use super::capability::Capability;

pub trait Clocksource: Capability {
    fn now_ns(&self) -> u64;
}
