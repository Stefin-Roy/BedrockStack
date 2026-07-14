use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;
use spin::Mutex;

use super::traits::{BlockDevice, IoBuffer, IoCompletions, IoRequest};

const CACHE_SIZE: usize = 4096;

struct CachedSector {
    data: [u8; 512],
    access_gen: u64,
}

pub struct CachedDevice {
    inner: Arc<dyn BlockDevice>,
    cache: Mutex<BlockCache>,
}

struct BlockCache {
    sectors: HashMap<u64, CachedSector>,
    gen_counter: u64,
}

impl BlockCache {
    fn new() -> Self {
        BlockCache {
            sectors: HashMap::new(),
            gen_counter: 0,
        }
    }

    fn touch(&mut self, lba: u64) {
        let g = self.gen_counter;
        self.gen_counter = g.wrapping_add(1);
        if let Some(entry) = self.sectors.get_mut(&lba) {
            entry.access_gen = g;
        }
    }

    fn maybe_evict(&mut self) {
        if self.sectors.len() < CACHE_SIZE {
            return;
        }
        let target = CACHE_SIZE - CACHE_SIZE / 4;
        let mut evictable: Vec<(u64, u64)> = self.sectors.iter()
            .map(|(lba, entry)| (*lba, entry.access_gen))
            .collect();
        evictable.sort_by_key(|&(_, g)| g);
        let n = self.sectors.len().saturating_sub(target);
        for (lba, _) in evictable.iter().take(n) {
            self.sectors.remove(lba);
        }
    }

    fn read(&mut self, device: &dyn BlockDevice, lba: u64) -> Result<&[u8; 512], ()> {
        if !self.sectors.contains_key(&lba) {
            let mut buf = [0u8; 512];
            let req = IoRequest { lba, count: 1, buffer: IoBuffer::Buf(&mut buf), is_write: false };
            let c = device.submit(&[req]).map_err(|_| ())?;
            if !c.all_ok() { return Err(()); }
            self.maybe_evict();
            self.sectors.insert(lba, CachedSector { data: buf, access_gen: 0 });
        }
        self.touch(lba);
        Ok(&self.sectors.get(&lba).unwrap().data)
    }

    fn read_into(&mut self, device: &dyn BlockDevice, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), ()> {
        if count <= 1 {
            let data = self.read(device, lba)?;
            buf[..data.len()].copy_from_slice(data);
            return Ok(());
        }
        // For multi-sector reads, bypass the cache and go directly
        let req = IoRequest { lba, count, buffer: IoBuffer::Buf(buf), is_write: false };
        let c = device.submit(&[req]).map_err(|_| ())?;
        if !c.all_ok() { return Err(()); }
        Ok(())
    }

    fn write(&mut self, device: &dyn BlockDevice, lba: u64, count: u32, buf: &[u8]) -> Result<(), ()> {
        if count <= 1 && buf.len() == 512 {
            let mut sector = [0u8; 512];
            sector.copy_from_slice(buf);
            self.maybe_evict();
            self.sectors.insert(lba, CachedSector { data: sector, access_gen: 0 });
            self.touch(lba);
        }
        // Always write-through to the device
        let req = IoRequest { lba, count, buffer: IoBuffer::ConstBuf(buf), is_write: true };
        let c = device.submit(&[req]).map_err(|_| ())?;
        if !c.all_ok() { return Err(()); }
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
        match &r.buffer {
            IoBuffer::Buf(buf) => {
                // buf is &&mut [u8]; need &mut [u8]
                let buf_len = buf.len();
                let buf_ptr = buf.as_ptr() as *mut u8;
                let mut_buf = unsafe { &mut *core::ptr::slice_from_raw_parts_mut(buf_ptr, buf_len) };
                cache.read_into(&*self.inner, r.lba, r.count, mut_buf)
            }
            _ => Err(()),
        }
    }
}

impl CachedDevice {
    pub fn new(inner: Arc<dyn BlockDevice>) -> Arc<Self> {
        Arc::new(CachedDevice {
            inner,
            cache: Mutex::new(BlockCache::new()),
        })
    }
}

impl BlockDevice for CachedDevice {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str> {
        let mut cache = self.cache.lock();
        let mut completed = 0u32;
        let mut errors = 0u32;
        for r in reqs {
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
