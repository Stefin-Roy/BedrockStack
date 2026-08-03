pub trait PciConfigSpace: Send + Sync {
    fn read8(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u8;
    fn read16(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u16;
    fn read32(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16) -> u32;
    fn write8(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u8);
    fn write16(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u16);
    fn write32(&self, seg: u16, bus: u8, dev: u8, func: u8, off: u16, val: u32);
}
