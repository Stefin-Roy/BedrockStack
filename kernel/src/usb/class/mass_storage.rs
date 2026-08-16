use alloc::sync::Arc;
use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoCompletions, IoRequest};
use crate::usb::xhci::device;
use crate::usb::xhci::memory::TrbRing;

const CBW_SIGNATURE: u32 = 0x43425355;
const CSW_SIGNATURE: u32 = 0x53425355;

const DIR_IN: u8 = 0x80;
const DIR_OUT: u8 = 0x00;

/// The xHCI layer caps a bulk transfer at 64 KiB per TRB (`device.rs`), so a
/// single SCSI command may carry at most this many sectors.
const MAX_SCSI_SECTORS: u32 = 64 * 1024 / 512;
const DATA_BUFFER_PAGES: usize = 16;

#[repr(C, packed)]
struct Cbw {
    d_cbw_signature: u32,
    d_cbw_tag: u32,
    d_cbw_data_transfer_length: u32,
    bm_cbw_flags: u8,
    b_cbw_lun: u8,
    b_cbwcb_length: u8,
    cbwcb: [u8; 16],
}

#[repr(C, packed)]
struct Csw {
    d_csw_signature: u32,
    d_csw_tag: u32,
    d_csw_data_residue: u32,
    b_csw_status: u8,
}

fn scsi_read10_cdb(lba: u32, count: u16) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x28;
    cdb[2] = (lba >> 24) as u8;
    cdb[3] = (lba >> 16) as u8;
    cdb[4] = (lba >> 8) as u8;
    cdb[5] = lba as u8;
    cdb[7] = (count >> 8) as u8;
    cdb[8] = count as u8;
    cdb
}

fn scsi_write10_cdb(lba: u32, count: u16) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x2A;
    cdb[2] = (lba >> 24) as u8;
    cdb[3] = (lba >> 16) as u8;
    cdb[4] = (lba >> 8) as u8;
    cdb[5] = lba as u8;
    cdb[7] = (count >> 8) as u8;
    cdb[8] = count as u8;
    cdb
}

fn scsi_read_capacity10_cdb() -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x25;
    cdb
}

fn scsi_inquiry_cdb() -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x12;
    cdb[4] = 36;
    cdb
}

fn scsi_test_unit_ready_cdb() -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x00;
    cdb
}

struct UsbMassStorageInner {
    doorbell_va: u64,
    slot_id: u8,
    bulk_out_dci: u8,
    bulk_in_dci: u8,
    bulk_out_ring: TrbRing,
    bulk_in_ring: TrbRing,
    tag: u32,
    /// Large contiguous DMA bounce buffer for bulk data (CBW/CSW live in
    /// their own pages so they never collide with multi-sector data).
    data_phys: u64,
    data_va: u64,
    data_size: usize,
    cbw_phys: u64,
    cbw_va: u64,
    csw_phys: u64,
    csw_va: u64,
}

impl UsbMassStorageInner {
    fn bot_send_cbw(&mut self, cbw_bytes: &[u8; 31]) -> Result<(), &'static str> {
        unsafe {
            core::ptr::copy_nonoverlapping(cbw_bytes.as_ptr(), self.cbw_va as *mut u8, 31);
        }
        device::submit_bulk(
            &mut self.bulk_out_ring,
            self.doorbell_va,
            self.slot_id,
            self.bulk_out_dci,
            self.cbw_phys,
            31,
        )
    }

    fn bot_receive_csw(&mut self, csw: &mut Csw) -> Result<(), &'static str> {
        device::submit_bulk(
            &mut self.bulk_in_ring,
            self.doorbell_va,
            self.slot_id,
            self.bulk_in_dci,
            self.csw_phys,
            13,
        )?;
        let csw_bytes = unsafe { core::slice::from_raw_parts(self.csw_va as *const u8, 13) };
        unsafe {
            core::ptr::copy_nonoverlapping(csw_bytes.as_ptr(), csw as *mut Csw as *mut u8, 13);
        }
        if csw.d_csw_signature != CSW_SIGNATURE {
            return Err("bad CSW signature");
        }
        if csw.b_csw_status != 0 {
            return Err("CSW status failed");
        }
        Ok(())
    }

    fn bot_transfer_data_in(&mut self, buf_phys: u64, len: u32) -> Result<(), &'static str> {
        device::submit_bulk(
            &mut self.bulk_in_ring,
            self.doorbell_va,
            self.slot_id,
            self.bulk_in_dci,
            buf_phys,
            len,
        )
    }

    fn bot_transfer_data_out(&mut self, buf_phys: u64, len: u32) -> Result<(), &'static str> {
        device::submit_bulk(
            &mut self.bulk_out_ring,
            self.doorbell_va,
            self.slot_id,
            self.bulk_out_dci,
            buf_phys,
            len,
        )
    }

    fn do_scsi_command(
        &mut self,
        cdb: &[u8; 16],
        data_phys: u64,
        data_len: u32,
        dir_in: bool,
    ) -> Result<(), &'static str> {
        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1);

        let cbw = Cbw {
            d_cbw_signature: CBW_SIGNATURE,
            d_cbw_tag: tag,
            d_cbw_data_transfer_length: data_len,
            bm_cbw_flags: if dir_in { DIR_IN } else { DIR_OUT },
            b_cbw_lun: 0,
            b_cbwcb_length: 10,
            cbwcb: *cdb,
        };

        let cbw_bytes: [u8; 31] = unsafe { core::mem::transmute(cbw) };
        self.bot_send_cbw(&cbw_bytes)?;

        if data_len > 0 {
            if dir_in {
                self.bot_transfer_data_in(data_phys, data_len)?;
            } else {
                self.bot_transfer_data_out(data_phys, data_len)?;
            }
        }

        let mut csw = Csw {
            d_csw_signature: 0,
            d_csw_tag: 0,
            d_csw_data_residue: 0,
            b_csw_status: 0,
        };
        self.bot_receive_csw(&mut csw)?;

        Ok(())
    }
}

pub struct UsbMassStorageDevice {
    inner: Mutex<UsbMassStorageInner>,
    sector_count: u64,
    model: [u8; 32],
}

impl UsbMassStorageDevice {
    pub fn new(
        doorbell_va: u64,
        slot_id: u8,
        bulk_out_dci: u8,
        bulk_in_dci: u8,
        bulk_out_ring: TrbRing,
        bulk_in_ring: TrbRing,
        dma: &dyn crate::services::dma::DmaAllocator,
    ) -> Result<Arc<Self>, &'static str> {
        let data_buf = dma
            .alloc_contiguous(DATA_BUFFER_PAGES)
            .ok_or("OOM for USB MSD data buffer")?;
        let cbw_page = dma.alloc_page().ok_or("OOM for USB MSD CBW page")?;
        let csw_page = dma.alloc_page().ok_or("OOM for USB MSD CSW page")?;

        let inner = Mutex::new(UsbMassStorageInner {
            doorbell_va,
            slot_id,
            bulk_out_dci,
            bulk_in_dci,
            bulk_out_ring,
            bulk_in_ring,
            tag: 1,
            data_phys: data_buf.phys,
            data_va: data_buf.virt,
            data_size: data_buf.size,
            cbw_phys: cbw_page.phys,
            cbw_va: cbw_page.virt,
            csw_phys: csw_page.phys,
            csw_va: csw_page.virt,
        });

        let mut model = [0u8; 32];
        let sector_count: u64;

        {
            let mut inner_ref = inner.lock();
            let dp = inner_ref.data_phys;
            let dv = inner_ref.data_va;

            inner_ref.do_scsi_command(&scsi_inquiry_cdb(), dp, 36, true)?;
            let inquiry = unsafe { core::slice::from_raw_parts(dv as *const u8, 36) };
            for i in 0..28 {
                if 8 + i < 36 {
                    model[i] = inquiry[8 + i];
                }
            }

            inner_ref.do_scsi_command(&scsi_read_capacity10_cdb(), dp, 8, true)?;
            let cap = unsafe { core::slice::from_raw_parts(dv as *const u8, 8) };
            let total_blocks = u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]);
            let block_size = u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]);
            if block_size != 512 {
                return Err("unsupported block size");
            }
            sector_count = (total_blocks as u64) + 1;
        }

        let dev = UsbMassStorageDevice {
            inner,
            sector_count,
            model,
        };

        let model_str = core::str::from_utf8(&dev.model).unwrap_or("?");
        SerialPort::puts("[usb_msd] ");
        SerialPort::puts(model_str);
        SerialPort::puts(" sectors=");
        SerialPort::put_u64(dev.sector_count);
        SerialPort::puts("\n");

        Ok(Arc::new(dev))
    }
}

use crate::usb::class::driver::{BoundUsbDevice, InterfaceResources, UsbClassDriver};
use crate::usb::usb::CLASS_MASS_STORAGE;

/// Class driver for the Mass Storage Class (Bulk-Only Transport).
pub struct MassStorageDriver;

impl UsbClassDriver for MassStorageDriver {
    fn name(&self) -> &str {
        "usb-mass-storage"
    }

    fn probe(&self, iface_class: u8, _subclass: u8, _protocol: u8) -> bool {
        iface_class == CLASS_MASS_STORAGE
    }

    fn init_interface(
        &self,
        res: InterfaceResources,
        dma: &dyn crate::services::dma::DmaAllocator,
        _ep0_ring: &mut TrbRing,
    ) -> Result<BoundUsbDevice, &'static str> {
        let bulk_in = res.bulk_in.ok_or("MSD needs bulk IN endpoint")?;
        let bulk_out = res.bulk_out.ok_or("MSD needs bulk OUT endpoint")?;
        let dev = UsbMassStorageDevice::new(
            res.doorbell_va,
            res.slot_id,
            bulk_out.dci,
            bulk_in.dci,
            bulk_out.ring,
            bulk_in.ring,
            dma,
        )?;
        Ok(BoundUsbDevice::Block(dev))
    }
}

impl BlockDevice for UsbMassStorageDevice {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str> {
        let mut inner = self.inner.lock();
        let mut completed = 0u32;
        let mut errors = 0u32;

        for req in reqs {
            let count = req.count;
            if count == 0 {
                completed += 1;
                continue;
            }

            let bytes = (count as usize) * 512;
            let (buf_vaddr, buf_size) = match &req.buffer {
                IoBuffer::Buf(buf) => (buf.as_ptr() as u64, buf.len()),
                IoBuffer::ConstBuf(buf) => (buf.as_ptr() as u64, buf.len()),
                IoBuffer::Phys(pa, sz) => (*pa, *sz),
            };

            if buf_size < bytes {
                errors += 1;
                continue;
            }

            let max_sectors = ((inner.data_size / 512) as u32).min(MAX_SCSI_SECTORS);
            let data_phys = inner.data_phys;
            let data_va = inner.data_va;

            if req.is_write {
                let mut i = 0u32;
                while i < count {
                    let chunk = (count - i).min(max_sectors);
                    let chunk_bytes = (chunk as usize) * 512;
                    let lba = req.lba + i as u64;
                    let src = buf_vaddr + (i as usize * 512) as u64;
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src as *const u8,
                            data_va as *mut u8,
                            chunk_bytes,
                        );
                    }
                    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                    inner.do_scsi_command(
                        &scsi_write10_cdb(lba as u32, chunk as u16),
                        data_phys,
                        chunk_bytes as u32,
                        false,
                    )?;
                    i += chunk;
                }
            } else {
                let mut i = 0u32;
                while i < count {
                    let chunk = (count - i).min(max_sectors);
                    let chunk_bytes = (chunk as usize) * 512;
                    let lba = req.lba + i as u64;
                    inner.do_scsi_command(
                        &scsi_read10_cdb(lba as u32, chunk as u16),
                        data_phys,
                        chunk_bytes as u32,
                        true,
                    )?;
                    let dst = buf_vaddr + (i as usize * 512) as u64;
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            data_va as *const u8,
                            dst as *mut u8,
                            chunk_bytes,
                        );
                    }
                    i += chunk;
                }
            }
            completed += 1;
        }

        Ok(IoCompletions { completed, errors })
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn model_string(&self) -> &str {
        let end = self.model.iter().position(|&c| c == 0).unwrap_or(32);
        core::str::from_utf8(&self.model[..end]).unwrap_or("(bad utf8)")
    }
}
