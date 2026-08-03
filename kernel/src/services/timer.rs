pub trait TimerProvider: Send + Sync {
    fn now_ns(&self) -> u64;
    fn sleep_ns(&self, ns: u64);
    fn register_tick_handler(&self, handler: fn());
}
