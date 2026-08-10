
pub trait Clocksource: Send + Sync {
    fn now_ns(&self) -> u64;
}
