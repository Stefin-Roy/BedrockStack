pub trait BlockDevice: Sync {
    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), &'static str>;
    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), &'static str>;
    fn sector_count(&self) -> u64;
    fn model_string(&self) -> &str;
}
