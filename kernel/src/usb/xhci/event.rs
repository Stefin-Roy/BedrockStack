use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::drivers::serial::SerialPort;

static XHCI_IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn irq_count() -> u64 {
    XHCI_IRQ_COUNT.load(Ordering::Relaxed)
}

static XHCI_ER_VADDR: AtomicU64 = AtomicU64::new(0);
static XHCI_ER_PADDR: AtomicU64 = AtomicU64::new(0);
static XHCI_ER_SIZE: AtomicU32 = AtomicU32::new(0);
static XHCI_ER_DEQUEUE: AtomicU16 = AtomicU16::new(0);
static XHCI_ER_CYCLE: AtomicU32 = AtomicU32::new(1);
static XHCI_RT_VA: AtomicU64 = AtomicU64::new(0);

pub fn set_event_ring_info(vaddr: u64, paddr: u64, trb_count: u32, dequeue_index: u16) {
    XHCI_ER_VADDR.store(vaddr, Ordering::Relaxed);
    XHCI_ER_PADDR.store(paddr, Ordering::Relaxed);
    XHCI_ER_SIZE.store(trb_count, Ordering::Relaxed);
    XHCI_ER_DEQUEUE.store(dequeue_index, Ordering::Relaxed);
    XHCI_ER_CYCLE.store(1, Ordering::Relaxed);
}

pub fn set_erdp_register_va(rt_va: u64) {
    XHCI_RT_VA.store(rt_va, Ordering::Relaxed);
}

static XHCI_OP_VA: AtomicU64 = AtomicU64::new(0);

pub fn set_op_base_va(op_va: u64) {
    XHCI_OP_VA.store(op_va, Ordering::Relaxed);
}

fn erdp_ptr(paddr: u64, dequeue_index: u16) -> u64 {
    (paddr + (dequeue_index as u64) * 16) | (1 << 3)
}

pub fn drain_pending_and_clear_intr() {
    let er_vaddr = XHCI_ER_VADDR.load(Ordering::Relaxed);
    if er_vaddr == 0 { return; }
    consume_pending_events();
    let op_va = XHCI_OP_VA.load(Ordering::Relaxed);
    if op_va != 0 {
        unsafe {
            core::ptr::write_volatile((op_va + 0x04) as *mut u32, 1 << 3);
        }
    }
    let rt_va = XHCI_RT_VA.load(Ordering::Relaxed);
    if rt_va != 0 {
        unsafe {
            let iman = core::ptr::read_volatile((rt_va + 0x20) as *const u32);
            core::ptr::write_volatile((rt_va + 0x20) as *mut u32, (iman & 2) | 1);
        }
    }
}

pub fn xhci_irq_handler() {
    XHCI_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    crate::arch::x86_64::idt::verify_integrity();
    let op_va = XHCI_OP_VA.load(Ordering::Relaxed);
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
    let slot_id = ((status >> 8) & 0xFF) as u8;
    let trb_type = (control >> 10) & 0x3F;
    (param as u32, completion_code, slot_id, trb_type)
}

static LAST_CMD_STATE: AtomicU64 = AtomicU64::new(0);

pub fn consume_pending_events() {
    let er_vaddr = XHCI_ER_VADDR.load(Ordering::Relaxed);
    if er_vaddr == 0 {
        return;
    }
    let er_trb_count = XHCI_ER_SIZE.load(Ordering::Relaxed);
    let mut dequeue = XHCI_ER_DEQUEUE.load(Ordering::Relaxed);
    let mut expected_cycle = XHCI_ER_CYCLE.load(Ordering::Relaxed);

    for _ in 0..er_trb_count {
        let trb_va = er_vaddr + (dequeue as u64) * 16;
        let control = unsafe { core::ptr::read_volatile((trb_va + 12) as *const u32) };

        if (control & 1) != expected_cycle {
            break;
        }

        let trb_type = ((control >> 10) & 0x3F) as u8;

        match trb_type {
            33 => {
                let param = unsafe { core::ptr::read_volatile(trb_va as *const u64) };
                let status = unsafe { core::ptr::read_volatile((trb_va + 8) as *const u32) };
                let cc = (status >> 24) as u8;
                let slot_id = ((status >> 8) & 0xFF) as u8;
                // Atomically publish all three fields with the seen flag (bit 63).
                let state = (slot_id as u64) | ((cc as u64) << 8) | ((param as u64) << 16) | (1u64 << 63);
                LAST_CMD_STATE.store(state, Ordering::Release);
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

    XHCI_ER_DEQUEUE.store(dequeue, Ordering::Relaxed);
    XHCI_ER_CYCLE.store(expected_cycle, Ordering::Relaxed);

    // Always update ERDP to the current dequeue position and clear IMAN
    // IP, even when no new events were processed.  Failing to clear IP
    // on a spurious IRQ causes an infinite interrupt storm.
    let rt_va = XHCI_RT_VA.load(Ordering::Relaxed);
    let er_paddr = XHCI_ER_PADDR.load(Ordering::Relaxed);
    if rt_va != 0 && er_paddr != 0 {
        let erdp_off = rt_va + 0x38;
        let erdp_val = erdp_ptr(er_paddr, dequeue);
        unsafe {
            core::ptr::write_volatile(erdp_off as *mut u32, erdp_val as u32);
            core::ptr::write_volatile((erdp_off + 4) as *mut u32, (erdp_val >> 32) as u32);
        }
    }
    if rt_va != 0 {
        let iman_off = rt_va + 0x20;
        unsafe {
            let iman = core::ptr::read_volatile(iman_off as *const u32);
            core::ptr::write_volatile(iman_off as *mut u32, (iman & 2) | 1);
        }
    }
}

pub fn last_command_completion() -> Option<(u8, u8, u32)> {
    let state = LAST_CMD_STATE.swap(0, Ordering::AcqRel);
    if state & (1u64 << 63) != 0 {
        let slot = state as u8;
        let cc = (state >> 8) as u8;
        let param = (state >> 16) as u32;
        Some((slot, cc, param))
    } else {
        None
    }
}
