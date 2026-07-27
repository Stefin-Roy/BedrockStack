use crate::drivers::serial::SerialPort;

static mut XHCI_IRQ_COUNT: u64 = 0;

pub fn irq_count() -> u64 {
    unsafe { XHCI_IRQ_COUNT }
}

static XHCI_ER_VADDR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static XHCI_ER_PADDR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static XHCI_ER_SIZE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static XHCI_ER_DEQUEUE: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
static XHCI_ER_CYCLE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
static XHCI_RT_VA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn set_event_ring_info(vaddr: u64, paddr: u64, trb_count: u32, dequeue_index: u16) {
    XHCI_ER_VADDR.store(vaddr, core::sync::atomic::Ordering::Relaxed);
    XHCI_ER_PADDR.store(paddr, core::sync::atomic::Ordering::Relaxed);
    XHCI_ER_SIZE.store(trb_count, core::sync::atomic::Ordering::Relaxed);
    XHCI_ER_DEQUEUE.store(dequeue_index, core::sync::atomic::Ordering::Relaxed);
    XHCI_ER_CYCLE.store(1, core::sync::atomic::Ordering::Relaxed);
}

pub fn set_erdp_register_va(rt_va: u64) {
    XHCI_RT_VA.store(rt_va, core::sync::atomic::Ordering::Relaxed);
}

static XHCI_OP_VA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn set_op_base_va(op_va: u64) {
    XHCI_OP_VA.store(op_va, core::sync::atomic::Ordering::Relaxed);
}

fn erdp_ptr(paddr: u64, dequeue_index: u16) -> u64 {
    paddr + (dequeue_index as u64) * 16
}

pub fn drain_pending_and_clear_intr() {
    let er_vaddr = XHCI_ER_VADDR.load(core::sync::atomic::Ordering::Relaxed);
    if er_vaddr == 0 { return; }
    consume_pending_events();
    let op_va = XHCI_OP_VA.load(core::sync::atomic::Ordering::Relaxed);
    if op_va != 0 {
        unsafe {
            core::ptr::write_volatile((op_va + 0x04) as *mut u32, 1 << 3);
        }
    }
    let rt_va = XHCI_RT_VA.load(core::sync::atomic::Ordering::Relaxed);
    if rt_va != 0 {
        unsafe {
            let iman = core::ptr::read_volatile((rt_va + 0x20) as *const u32);
            core::ptr::write_volatile((rt_va + 0x20) as *mut u32, (iman & 2) | 1);
        }
    }
}

pub fn xhci_irq_handler() {
    unsafe { XHCI_IRQ_COUNT += 1; }
    crate::arch::x86_64::idt::verify_integrity();
    let op_va = XHCI_OP_VA.load(core::sync::atomic::Ordering::Relaxed);
    if op_va != 0 {
        unsafe {
            core::ptr::write_volatile((op_va + 0x04) as *mut u32, 1 << 3);
        }
    }
    consume_pending_events();
}

pub fn read_event_completion_at(trb_va: u64) -> (u32, u8, u8, u32) {
    let param = unsafe { core::ptr::read_volatile(trb_va as *const u64) };
    let status = unsafe { core::ptr::read_volatile((trb_va + 8) as *const u32) };
    let control = unsafe { core::ptr::read_volatile((trb_va + 12) as *const u32) };
    let completion_code = (status >> 24) as u8;
    let slot_id = ((status >> 16) & 0xFF) as u8;
    let trb_type = (control >> 10) & 0x3F;
    (param as u32, completion_code, slot_id, trb_type)
}

static LAST_CMD_SLOT_ID: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0xFF);
static LAST_CMD_COMPLETION: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0xFF);
static LAST_CMD_PARAM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static LAST_CMD_SEEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn consume_pending_events() {
    let er_vaddr = XHCI_ER_VADDR.load(core::sync::atomic::Ordering::Relaxed);
    if er_vaddr == 0 {
        return;
    }
    let er_trb_count = XHCI_ER_SIZE.load(core::sync::atomic::Ordering::Relaxed);
    let mut dequeue = XHCI_ER_DEQUEUE.load(core::sync::atomic::Ordering::Relaxed);
    let mut expected_cycle = XHCI_ER_CYCLE.load(core::sync::atomic::Ordering::Relaxed);

    let mut processed = false;

    for _ in 0..er_trb_count {
        let trb_va = er_vaddr + (dequeue as u64) * 16;
        let control = unsafe { core::ptr::read_volatile((trb_va + 12) as *const u32) };

        if (control & 1) != expected_cycle {
            break;
        }

        let trb_type = ((control >> 10) & 0x3F) as u8;
        processed = true;

        match trb_type {
            33 => {
                let param = unsafe { core::ptr::read_volatile(trb_va as *const u64) };
                let status = unsafe { core::ptr::read_volatile((trb_va + 8) as *const u32) };
                let cc = (status >> 24) as u8;
                let slot_id = ((status >> 16) & 0xFF) as u8;
                LAST_CMD_SLOT_ID.store(slot_id, core::sync::atomic::Ordering::Relaxed);
                LAST_CMD_COMPLETION.store(cc, core::sync::atomic::Ordering::Relaxed);
                LAST_CMD_PARAM.store(param as u32, core::sync::atomic::Ordering::Relaxed);
                LAST_CMD_SEEN.store(true, core::sync::atomic::Ordering::Relaxed);
            }
            34 => {
                let param = unsafe { core::ptr::read_volatile(trb_va as *const u64) };
                let port_id = (param >> 24) & 0xFF;
                SerialPort::puts("[xhci] evt: port change port=");
                SerialPort::put_u64(port_id);
                SerialPort::puts("\n");
            }
            37 | 32 => {}
            _ => {}
        }

        dequeue = (dequeue + 1) % er_trb_count as u16;
        if dequeue == 0 {
            expected_cycle ^= 1;
        }
    }

    XHCI_ER_DEQUEUE.store(dequeue, core::sync::atomic::Ordering::Relaxed);
    XHCI_ER_CYCLE.store(expected_cycle, core::sync::atomic::Ordering::Relaxed);

    if processed {
        let rt_va = XHCI_RT_VA.load(core::sync::atomic::Ordering::Relaxed);
        let er_paddr = XHCI_ER_PADDR.load(core::sync::atomic::Ordering::Relaxed);
        if rt_va != 0 && er_paddr != 0 {
            let erdp_off = rt_va + 0x38;
            let erdp_val = erdp_ptr(er_paddr, dequeue);
            unsafe {
                core::ptr::write_volatile(erdp_off as *mut u32, erdp_val as u32);
                core::ptr::write_volatile((erdp_off + 4) as *mut u32, (erdp_val >> 32) as u32);
            }
            let iman_off = rt_va + 0x20;
            unsafe {
                let iman = core::ptr::read_volatile(iman_off as *const u32);
                core::ptr::write_volatile(iman_off as *mut u32, (iman & 2) | 1);
            }
        }
    }
}

pub fn last_command_completion() -> Option<(u8, u8, u32)> {
    if LAST_CMD_SEEN.load(core::sync::atomic::Ordering::Relaxed) {
        let slot = LAST_CMD_SLOT_ID.load(core::sync::atomic::Ordering::Relaxed);
        let cc = LAST_CMD_COMPLETION.load(core::sync::atomic::Ordering::Relaxed);
        let param = LAST_CMD_PARAM.load(core::sync::atomic::Ordering::Relaxed);
        LAST_CMD_SEEN.store(false, core::sync::atomic::Ordering::Relaxed);
        Some((slot, cc, param))
    } else {
        None
    }
}
