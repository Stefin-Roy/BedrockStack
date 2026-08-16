use crate::services::dma::DmaAllocator;
use core::sync::atomic::{Ordering, fence};

pub const TRB_TYPE_NORMAL: u8 = 1;
pub const TRB_TYPE_SETUP_STAGE: u8 = 2;
pub const TRB_TYPE_DATA_STAGE: u8 = 3;
pub const TRB_TYPE_STATUS_STAGE: u8 = 4;
pub const TRB_TYPE_ISOCH: u8 = 5;
pub const TRB_TYPE_LINK: u8 = 6;
pub const TRB_TYPE_EVENT_DATA: u8 = 7;
pub const TRB_TYPE_NO_OP: u8 = 8;
pub const TRB_TYPE_ENABLE_SLOT: u8 = 9;
pub const TRB_TYPE_DISABLE_SLOT: u8 = 10;
pub const TRB_TYPE_ADDRESS_DEVICE: u8 = 11;
pub const TRB_TYPE_CONFIGURE_ENDPOINT: u8 = 12;
pub const TRB_TYPE_EVALUATE_CONTEXT: u8 = 13;
pub const TRB_TYPE_RESET_ENDPOINT: u8 = 14;
pub const TRB_TYPE_STOP_ENDPOINT: u8 = 15;
pub const TRB_TYPE_SET_TR_DEQUEUE: u8 = 16;
pub const TRB_TYPE_RESET_DEVICE: u8 = 17;
pub const TRB_TYPE_FORCE_EVENT: u8 = 18;
pub const TRB_TYPE_NEG_BANDWIDTH: u8 = 19;
pub const TRB_TYPE_SET_LATENCY: u8 = 20;
pub const TRB_TYPE_GET_PORT_BANDWIDTH: u8 = 21;
pub const TRB_TYPE_FORCE_HEADER: u8 = 22;
pub const TRB_TYPE_NO_OP_COMMAND: u8 = 23;

pub const TRB_CYCLE: u32 = 1 << 0;
pub const TRB_TC: u32 = 1 << 1;
pub const TRB_ISP: u32 = 1 << 2;
pub const TRB_NS: u32 = 1 << 3;
pub const TRB_CHAIN: u32 = 1 << 4;
pub const TRB_ENT: u32 = 1 << 1;
pub const TRB_BSR: u32 = 1 << 9;
pub const TRB_DC: u32 = 1 << 9;
pub const TRB_IOC: u32 = 1 << 5;
pub const TRB_IDT: u32 = 1 << 6;
pub const TRB_SIA: u32 = 1 << 31;

pub const TRB_DIR_IN: u32 = 1 << 16;

pub const LINK_TOGGLE_CYCLE: u32 = 1 << 1;

pub const EVT_TRANSFER: u32 = 32 << 10;
pub const EVT_COMMAND_COMPLETION: u32 = 33 << 10;
pub const EVT_PORT_STATUS_CHANGE: u32 = 34 << 10;
pub const EVT_MFINDEX_WRAP: u32 = 39 << 10;
pub const EVT_HOST_CONTROLLER: u32 = 37 << 10;
pub const EVT_DEVICE_NOTIFICATION: u32 = 38 << 10;

pub const CMD_INVALID: u16 = 0;
pub const CMD_SUCCESS: u16 = 1;
pub const CMD_DATA_BUFFER_ERROR: u16 = 2;
pub const CMD_BABBLE_DETECTED: u16 = 3;
pub const CMD_USB_TRANSACTION_ERROR: u16 = 4;
pub const CMD_TRB_ERROR: u16 = 5;
pub const CMD_STALL_ERROR: u16 = 6;
pub const CMD_RESOURCE_ERROR: u16 = 7;
pub const CMD_BANDWIDTH_ERROR: u16 = 8;
pub const CMD_NO_SLOTS_ERROR: u16 = 9;
pub const CMD_INVALID_STREAM_TYPE: u16 = 10;
pub const CMD_SLOT_NOT_ENABLED: u16 = 11;
pub const CMD_EP_NOT_ENABLED: u16 = 12;
pub const CMD_SHORT_PACKET: u16 = 13;
pub const CMD_RING_UNDERRUN: u16 = 14;
pub const CMD_RING_OVERRUN: u16 = 15;
pub const CMD_VF_ER_FULL: u16 = 16;
pub const CMD_PARAM_ERROR: u16 = 17;
pub const CMD_CONTEXT_STATE_ERROR: u16 = 18;

#[repr(C)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    pub fn new(parameter: u64, status: u32, control: u32) -> Self {
        Trb {
            parameter,
            status,
            control,
        }
    }

    pub fn trb_type(&self) -> u8 {
        let ctrl = unsafe { core::ptr::read_volatile(&self.control as *const u32) };
        ((ctrl >> 10) & 0x3F) as u8
    }

    pub fn cycle_bit(&self) -> bool {
        let ctrl = unsafe { core::ptr::read_volatile(&self.control as *const u32) };
        ctrl & TRB_CYCLE != 0
    }

    pub fn is_event(&self) -> bool {
        let t = self.trb_type();
        t >= 32 && t <= 47
    }
}

pub struct TrbRing {
    pub phys: u64,
    pub virt: u64,
    pub page_count: usize,
    pub trb_count: u16,
    enqueue_index: u16,
    cycle: u32,
}

impl TrbRing {
    pub fn new(dma: &dyn DmaAllocator, size: usize) -> Result<Self, &'static str> {
        let page_count = (size + 4095) / 4096;
        let buf = dma.alloc_contiguous(page_count).ok_or("OOM for TRB ring")?;
        let trb_count = (buf.size / 16) as u16;

        unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, buf.size) };

        // Write a valid Link TRB at the segment end so the xHC can wrap.
        let link_idx = (trb_count - 1) as usize;
        let link_va = buf.virt + (link_idx as u64) * 16;
        unsafe {
            core::ptr::write_volatile(link_va as *mut u64, buf.phys);
            core::ptr::write_volatile((link_va + 8) as *mut u32, 0);
            core::ptr::write_volatile(
                (link_va + 12) as *mut u32,
                (TRB_TYPE_LINK as u32) << 10 | LINK_TOGGLE_CYCLE | TRB_TC | 1,
            );
        }

        Ok(TrbRing {
            phys: buf.phys,
            virt: buf.virt,
            page_count,
            trb_count: trb_count - 1,
            enqueue_index: 0,
            cycle: 1,
        })
    }

    pub fn enqueue(&mut self, trb: &Trb) -> u64 {
        let idx = self.enqueue_index as usize;
        let va = self.virt + (idx as u64) * 16;
        unsafe {
            let control = (trb.control & !1) | self.cycle;
            core::ptr::write_volatile(va as *mut u64, trb.parameter);
            core::ptr::write_volatile((va + 8) as *mut u32, trb.status);
            core::ptr::write_volatile((va + 12) as *mut u32, control);
        }
        let phys = self.phys + (idx as u64) * 16;
        let next = self.enqueue_index + 1;
        if next >= self.trb_count {
            // Write the Link TRB with the *current* cycle bit so the
            // controller sees a valid TRB.  Then toggle for the new
            // segment and wrap the enqueue pointer.
            self.write_link_trb(self.trb_count as usize);
            self.enqueue_index = 0;
            self.cycle ^= 1;
        } else {
            self.enqueue_index = next;
        }
        phys
    }

    fn write_link_trb(&self, idx: usize) {
        let va = self.virt + (idx as u64) * 16;
        let link_phys = self.phys;
        unsafe {
            core::ptr::write_volatile(va as *mut u64, link_phys);
            core::ptr::write_volatile((va + 8) as *mut u32, 0);
            core::ptr::write_volatile(
                (va + 12) as *mut u32,
                (TRB_TYPE_LINK as u32) << 10 | LINK_TOGGLE_CYCLE | TRB_TC | self.cycle,
            );
        }
    }

    pub fn current_phys(&self) -> u64 {
        self.phys + (self.enqueue_index as u64) * 16
    }

    pub fn flush(&self) {
        fence(Ordering::SeqCst);
    }
}

pub fn make_setup_stage_trb(setup: &[u8; 8], trt: u32) -> Trb {
    let param = u64::from_le_bytes([
        setup[0], setup[1], setup[2], setup[3], setup[4], setup[5], setup[6], setup[7],
    ]);
    Trb::new(
        param,
        8,
        (TRB_TYPE_SETUP_STAGE as u32) << 10 | ((trt & 3) << 16) | TRB_IDT,
    )
}

pub fn make_data_stage_trb(phys: u64, len: u32, dir_in: bool) -> Trb {
    let mut control = (TRB_TYPE_DATA_STAGE as u32) << 10;
    if dir_in {
        control |= TRB_DIR_IN;
    }
    Trb::new(phys, len, control)
}

pub fn make_status_stage_trb(dir_in: bool) -> Trb {
    let mut control = (TRB_TYPE_STATUS_STAGE as u32) << 10;
    if !dir_in {
        control |= TRB_DIR_IN;
    }
    control |= TRB_IOC;
    Trb::new(0, 0, control)
}

pub fn make_enable_slot_trb() -> Trb {
    Trb::new(0, 0, (TRB_TYPE_ENABLE_SLOT as u32) << 10)
}

pub fn make_address_device_trb(input_ctx_phys: u64, slot_id: u8, bsr: bool) -> Trb {
    let mut control = (TRB_TYPE_ADDRESS_DEVICE as u32) << 10;
    if bsr {
        control |= TRB_BSR;
    }
    control |= (slot_id as u32) << 24;
    Trb::new(input_ctx_phys, 0, control)
}

pub fn make_configure_endpoint_trb(input_ctx_phys: u64, slot_id: u8, deconfigure: bool) -> Trb {
    let mut control = (TRB_TYPE_CONFIGURE_ENDPOINT as u32) << 10;
    if deconfigure {
        control |= TRB_DC;
    }
    control |= (slot_id as u32) << 24;
    Trb::new(input_ctx_phys, 0, control)
}

pub fn make_evaluate_context_trb(ctx_phys: u64, slot_id: u8) -> Trb {
    let mut control = (TRB_TYPE_EVALUATE_CONTEXT as u32) << 10;
    control |= (slot_id as u32) << 24;
    Trb::new(ctx_phys, 0, control)
}

pub fn make_normal_trb(data_phys: u64, len: u32) -> Trb {
    let mut control = (TRB_TYPE_NORMAL as u32) << 10;
    control |= TRB_IOC;
    Trb::new(data_phys, len & 0x1FFFF, control)
}

/// Like [`make_normal_trb`] but with explicit CHAIN/IOC control bits, for
/// chained multi-TRB transfer descriptors (e.g. an isochronous TD larger than
/// one burst): CHAIN=1 on every TRB of the TD except the last.
pub fn make_normal_trb_flags(data_phys: u64, len: u32, chain: bool, ioc: bool) -> Trb {
    let mut control = (TRB_TYPE_NORMAL as u32) << 10;
    if chain {
        control |= TRB_CHAIN;
    }
    if ioc {
        control |= TRB_IOC;
    }
    Trb::new(data_phys, len & 0x1FFFF, control)
}

/// Build an isochronous TRB (spec Tables 6-32/33/34).  A single transfer may
/// span multiple TRBs chained via CHAIN=1 (see [`make_normal_trb_flags`]).
///
/// * `frame_id` — 11-bit frame in which to schedule the transfer, meaningful
///   only when `sia` (Schedule-Immediate-Activation) is clear.
/// * `tbc` — TD toggle bit (isoch IN endpoints only; 0 for OUT).
/// * `tlbpc` — last-burst-packet count (0 unless the TD exceeds one burst);
///   for the common single-burst case it equals the TD Size / packet count.
/// * `ioc` — interrupt-on-completion, so the (possibly overrun/underrun)
///   completion code is observable.
pub fn make_isoch_trb(
    data_phys: u64,
    len: u32,
    frame_id: u16,
    sia: bool,
    tbc: bool,
    tlbpc: u8,
    ioc: bool,
) -> Trb {
    // dw2: Transfer Length (bits 0-16); TD Size @17-21 and Interrupter
    // Target @22-31 are 0 (single-TD, interrupter 0).
    let status: u32 = len & 0x1FFFF;
    // dw3: TBC @7, TRB Status @8, BEI @9, Type @10-15, TLBPC @16-19,
    // Frame ID @20-30, SIA @31.
    let mut control = (TRB_TYPE_ISOCH as u32) << 10;
    if tbc {
        control |= 1 << 7;
    }
    if tlbpc != 0 {
        control |= (tlbpc as u32 & 0xF) << 16;
    }
    control |= (frame_id as u32 & 0x7FF) << 20;
    if sia {
        control |= TRB_SIA;
    }
    if ioc {
        control |= TRB_IOC;
    }
    Trb::new(data_phys, status, control)
}

pub fn make_no_op_command_trb() -> Trb {
    Trb::new(0, 0, (TRB_TYPE_NO_OP_COMMAND as u32) << 10 | TRB_IOC)
}

pub fn make_disable_slot_trb(slot_id: u8) -> Trb {
    let mut control = (TRB_TYPE_DISABLE_SLOT as u32) << 10;
    control |= (slot_id as u32) << 24;
    Trb::new(0, 0, control)
}
