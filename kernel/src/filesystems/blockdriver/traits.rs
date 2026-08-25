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
    fn sector_count(&self) -> u64;
    fn model_string(&self) -> &str;

    /// Logical sector size in bytes. All `IoRequest` counts are denominated
    /// in units of this size. Defaults to 512 for legacy devices.
    fn sector_size(&self) -> usize {
        512
    }

    /// Flush any device-internal write cache (e.g. ATA write cache).
    /// Returns once data is durable from this layer's perspective.
    /// Default: no-op for devices with no internal cache.
    fn sync(&self) -> Result<(), &'static str> {
        Ok(())
    }
}
