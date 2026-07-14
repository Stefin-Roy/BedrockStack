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
}
