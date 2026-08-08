use alloc::boxed::Box;

pub enum IoBuffer<'a> {
    Buf(&'a mut [u8]),
    ConstBuf(&'a [u8]),
    Phys(u64, usize),
}

pub struct IoRequest<'a> {
    pub lba: u64,
    pub count: u32,
    pub buffer: IoBuffer<'a>,
    pub is_write: bool,
}

pub struct IoCompletions {
    pub completed: u32,
    pub errors: u32,
}

impl IoCompletions {
    pub fn all_ok(&self) -> bool {
        self.errors == 0 && self.completed != 0
    }
}

pub trait BlockDevice: Send + Sync {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str>;

    /// Submit without blocking. Returns `Ok(true)` if the request completed
    /// synchronously, `Ok(false)` if it is now in flight (the `on_done` callback
    /// fires exactly once from device IRQ context when it completes), or `Err`
    /// if async is unsupported / the request failed. The caller MUST keep the
    /// request buffer's memory alive until `on_done` fires.
    fn submit_async(
        &self,
        reqs: &[IoRequest],
        on_done: Box<dyn Fn(IoCompletions) + Send>,
    ) -> Result<bool, &'static str> {
        let _ = (reqs, on_done);
        Err("async not supported")
    }

    fn sector_count(&self) -> u64;
    fn model_string(&self) -> &str;
}
