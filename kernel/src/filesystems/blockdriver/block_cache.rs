use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::filesystems::vfs::irq::IrqMutex;
use super::traits::{BlockDevice, IoBuffer, IoCompletions, IoRequest};

const CACHE_SIZE: usize = 4096;

struct CachedSector {
    data: [u8; 512],
}

pub struct CachedDevice {
    inner: Arc<dyn BlockDevice>,
    cache: IrqMutex<BlockCache>,
}

struct BlockCache {
    sectors: HashMap<u64, CachedSector>,
    clock: Vec<u64>,
    clock_hand: usize,
}

impl BlockCache {
    fn new() -> Self {
        BlockCache {
            sectors: HashMap::new(),
            clock: Vec::with_capacity(CACHE_SIZE),
            clock_hand: 0,
        }
    }

    fn maybe_evict(&mut self) {
        if self.sectors.len() < CACHE_SIZE {
            return;
        }
        let target = CACHE_SIZE - CACHE_SIZE / 4;
        while self.sectors.len() > target {
            if self.clock.is_empty() { break; }
            if self.clock_hand >= self.clock.len() {
                self.clock_hand = 0;
            }
            let lba = self.clock[self.clock_hand];
            self.sectors.remove(&lba);
            self.clock.swap_remove(self.clock_hand);
        }
    }

    fn read(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&[u8; 512], ()> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; 512];
            let req = IoRequest { lba, count: 1, buffer: IoBuffer::Buf(&mut buf), is_write: false };
            let c = device.submit(&[req]).map_err(|_| ())?;
            if !c.all_ok() { return Err(()); }
            self.maybe_evict();
            self.sectors.insert(lba, CachedSector { data: buf });
            self.clock.push(lba);
        }
        match self.sectors.get(&lba) {
            Some(sector) => Ok(&sector.data),
            None => Err(()),
        }
    }

    fn read_raw(&mut self, device: &dyn BlockDevice, lba: u64, count: u32, buf_ptr: *mut u8, buf_len: usize) -> Result<(), ()> {
        debug_assert!(count <= 1, "multi-sector reads bypass the cache in read_io");
        let data = self.read(device, lba)?;
        let copy_len = 512usize.min(buf_len);
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr, copy_len); }
        Ok(())
    }

    fn write(&mut self, device: &dyn BlockDevice, lba: u64, count: u32, buf: &[u8]) -> Result<(), ()> {
        // Write-through to device first
        let req = IoRequest { lba, count, buffer: IoBuffer::ConstBuf(buf), is_write: true };
        let c = device.submit(&[req]).map_err(|_| ())?;
        if !c.all_ok() { return Err(()); }
        // Only update cache after successful write
        if count <= 1 && buf.len() == 512 {
            let mut sector = [0u8; 512];
            sector.copy_from_slice(buf);
            if !self.sectors.contains_key(&lba) {
                self.maybe_evict();
                self.clock.push(lba);
            }
            self.sectors.insert(lba, CachedSector { data: sector });
        }
        Ok(())
    }
}

impl CachedDevice {
    fn write_io(&self, cache: &mut BlockCache, r: &IoRequest) -> Result<(), ()> {
        let buf = match &r.buffer {
            IoBuffer::ConstBuf(buf) => *buf,
            _ => return Err(()),
        };
        cache.write(&*self.inner, r.lba, r.count, buf)
    }

    fn read_io(&self, cache: &mut BlockCache, r: &IoRequest) -> Result<(), ()> {
        if r.count > 1 {
            // Multi-sector reads bypass the sector cache: forward the
            // caller's buffer straight to the backing device so drivers with
            // direct DMA (AHCI PRDT) can write zero-copy.  A temp Vec + copy
            // here just burns an alloc and a memcpy for no coherence benefit.
            let buffer = match &r.buffer {
                IoBuffer::Buf(buf) => {
                    let ptr = buf.as_ptr() as *mut u8;
                    let len = buf.len();
                    IoBuffer::Buf(unsafe { &mut *core::ptr::slice_from_raw_parts_mut(ptr, len) })
                }
                _ => return Err(()),
            };
            let req = IoRequest { lba: r.lba, count: r.count, buffer, is_write: false };
            let c = self.inner.submit(core::slice::from_ref(&req)).map_err(|_| ())?;
            return if c.all_ok() { Ok(()) } else { Err(()) };
        }
        let (buf_ptr, buf_len) = match &r.buffer {
            IoBuffer::Buf(buf) => (buf.as_ptr() as *mut u8, buf.len()),
            _ => return Err(()),
        };
        cache.read_raw(&*self.inner, r.lba, r.count, buf_ptr, buf_len)
    }
}

impl CachedDevice {
    pub fn new(inner: Arc<dyn BlockDevice>) -> Arc<Self> {
        Arc::new(CachedDevice {
            inner,
            cache: IrqMutex::new(BlockCache::new()),
        })
    }
}

impl BlockDevice for CachedDevice {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str> {
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::idt::verify_integrity();
        let mut cache = self.cache.lock();
        let mut completed = 0u32;
        let mut errors = 0u32;
        for r in reqs {
            // Physical-address buffers bypass the sector cache: the caller
            // owns a DMA-visible buffer at a fixed address, so forward the
            // request straight to the backing device (correct and avoids
            // aliasing cached sectors).
            if let IoBuffer::Phys(pa, sz) = &r.buffer {
                let phys_req = IoRequest {
                    lba: r.lba,
                    count: r.count,
                    buffer: IoBuffer::Phys(*pa, *sz),
                    is_write: r.is_write,
                };
                match self.inner.submit(core::slice::from_ref(&phys_req)) {
                    Ok(c) if c.all_ok() => completed += 1,
                    _ => errors += 1,
                }
                continue;
            }
            let result = if r.is_write {
                self.write_io(&mut cache, r)
            } else {
                self.read_io(&mut cache, r)
            };
            match result {
                Ok(()) => completed += 1,
                Err(()) => errors += 1,
            }
        }
        Ok(IoCompletions { completed, errors })
    }

    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }

    fn model_string(&self) -> &str {
        self.inner.model_string()
    }
}
