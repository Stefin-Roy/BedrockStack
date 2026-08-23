use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

macro_rules! usb_trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "usb_trace")]
        $($arg)*
    };
}

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
    if er_vaddr == 0 {
        return;
    }
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

static LAST_CMD_STATE: AtomicU64 = AtomicU64::new(0);
static LAST_TRANSFER_STATE: AtomicU64 = AtomicU64::new(0);

// ── Port-change event queue ───────────────────────────────────────
//
// PORT_CHANGE (type 34) events are pushed by the ISR and drained by the
// init path (and, once hot-plug lands, the idle loop).  Lock-free SPSC:
// the ISR is the only producer, the BSP init path the only consumer, both
// on the same CPU.  Port ids are u8, so a 64-entry ring never overflows in
// practice (max_ports <= 255, but a ring that wraps just coalesces).

const PORT_EVENT_CAP: usize = 64;

static PORT_EVENTS: [AtomicU8; PORT_EVENT_CAP] = [const { AtomicU8::new(0) }; PORT_EVENT_CAP];
static PORT_EVENTS_HEAD: AtomicU64 = AtomicU64::new(0);
static PORT_EVENTS_TAIL: AtomicU64 = AtomicU64::new(0);

fn port_events_push(port_id: u8) {
    let head = PORT_EVENTS_HEAD.load(Ordering::Acquire);
    let tail = PORT_EVENTS_TAIL.load(Ordering::Relaxed);
    if head - tail >= PORT_EVENT_CAP as u64 {
        return; // full — drop; the init drain re-reads PORTSC anyway
    }
    PORT_EVENTS[(head % PORT_EVENT_CAP as u64) as usize].store(port_id, Ordering::Relaxed);
    core::sync::atomic::fence(Ordering::Release);
    PORT_EVENTS_HEAD.store(head + 1, Ordering::Release);
}

/// Pop the next queued port id, or `None`.
pub fn take_port_change() -> Option<u8> {
    let tail = PORT_EVENTS_TAIL.load(Ordering::Acquire);
    let head = PORT_EVENTS_HEAD.load(Ordering::Relaxed);
    if tail == head {
        return None;
    }
    let port_id = PORT_EVENTS[(tail % PORT_EVENT_CAP as u64) as usize].load(Ordering::Relaxed);
    core::sync::atomic::fence(Ordering::Acquire);
    PORT_EVENTS_TAIL.store(tail + 1, Ordering::Release);
    Some(port_id)
}

/// True if any port-change events are pending.
pub fn port_change_pending() -> bool {
    PORT_EVENTS_HEAD.load(Ordering::Acquire) != PORT_EVENTS_TAIL.load(Ordering::Acquire)
}

// ── Transfer completion targets ────────────────────────────────────
//
// A small lock-free table routing transfer events for registered endpoints
// (interrupt IN/OUT, and later isochronous) straight to a per-target `ready`
// flag, so class drivers can poll for input without disturbing the shared
// LAST_TRANSFER_STATE slot (which the blocking control/bulk wait path
// owns).  The ISR (via `consume_pending_events`) is the only producer; the
// class driver's poll hook is the only consumer.  A completion arms the
// target only when its completion code is covered by the registered
// `cc_mask` — Success (1) | Short Packet (13) for interrupt, with the isoch
// overrun/underrun codes (10/11) additionally accepted where registered.
// Everything else falls through to LAST_TRANSFER_STATE unchanged.

const INTERRUPT_TARGET_CAP: usize = 8;

struct InterruptTarget {
    /// 0 = free; device slot ids are 1-based.  Released/published last so
    /// the ISR only ever observes a fully-formed target.
    slot_id: AtomicU8,
    dci: AtomicU8,
    buf_va: AtomicU64,
    cap: AtomicU32,
    /// Completion codes that arm `ready`, as a `1 << cc` bitmask.
    cc_mask: AtomicU32,
    /// Received transfer length, valid while `ready` is set.
    remaining: AtomicU32,
    ready: AtomicBool,
}

const fn new_interrupt_target() -> InterruptTarget {
    InterruptTarget {
        slot_id: AtomicU8::new(0),
        dci: AtomicU8::new(0),
        buf_va: AtomicU64::new(0),
        cap: AtomicU32::new(0),
        cc_mask: AtomicU32::new(0),
        remaining: AtomicU32::new(0),
        ready: AtomicBool::new(false),
    }
}

static INTERRUPT_TARGETS: [InterruptTarget; INTERRUPT_TARGET_CAP] =
    [const { new_interrupt_target() }; INTERRUPT_TARGET_CAP];

/// Register `(slot_id, dci)` as a transfer completion target so a transfer
/// event whose completion code is in `cc_mask` arms `ready`.  The caller is
/// responsible for keeping exactly one TRB in flight and calling
/// [`take_interrupt_completion`] before re-arming.
///
/// Returns `false` if the slot is invalid, the pair is already registered,
/// or the table is full.  (`dci` parity distinguishes the IN and OUT
/// endpoints of a device, so both may be registered independently.)
pub fn register_transfer_target(slot_id: u8, dci: u8, buf_va: u64, cap: u32, cc_mask: u32) -> bool {
    if slot_id == 0 || dci == 0 || cc_mask == 0 {
        return false;
    }
    for t in INTERRUPT_TARGETS.iter() {
        if t.slot_id.load(Ordering::Acquire) == slot_id && t.dci.load(Ordering::Relaxed) == dci {
            return false;
        }
    }
    for t in INTERRUPT_TARGETS.iter() {
        if t.slot_id.load(Ordering::Relaxed) != 0 {
            continue;
        }
        // Publish the identity fields first; the Release store of `slot_id`
        // publishes them to the ISR's Acquire load on the same entry.
        t.ready.store(false, Ordering::Relaxed);
        t.remaining.store(0, Ordering::Relaxed);
        t.buf_va.store(buf_va, Ordering::Relaxed);
        t.cap.store(cap, Ordering::Relaxed);
        t.cc_mask.store(cc_mask, Ordering::Relaxed);
        t.dci.store(dci, Ordering::Relaxed);
        t.slot_id.store(slot_id, Ordering::Release);
        return true;
    }
    false
}

/// Register `(slot_id, dci)` as an interrupt-endpoint completion target,
/// armed only by Success (1) or Short Packet (13) — the codes the blocking
/// wait path also accepts.
pub fn register_interrupt_target(slot_id: u8, dci: u8, buf_va: u64, cap: u32) -> bool {
    register_transfer_target(slot_id, dci, buf_va, cap, (1 << 1) | (1 << 13))
}

/// Poll-consumes a pending completion for `(slot_id, dci)`, returning the
/// received transfer length (the Transfer Event's residual `remaining`) if
/// the target's ready flag was armed, or `None` if no completion is pending.
pub fn take_interrupt_completion(slot_id: u8, dci: u8) -> Option<u32> {
    for t in INTERRUPT_TARGETS.iter() {
        if t.slot_id.load(Ordering::Acquire) == slot_id && t.dci.load(Ordering::Relaxed) == dci {
            if t.ready.swap(false, Ordering::AcqRel) {
                return Some(t.remaining.load(Ordering::Relaxed));
            }
            return None;
        }
    }
    None
}

/// Route a transfer completion to the registered target for `(slot_id,
/// ep_id)` when its completion code is covered by the target's `cc_mask`.
/// Returns `true` when the completion was consumed by a target and must NOT
/// be published to LAST_TRANSFER_STATE.
fn route_to_interrupt_target(slot_id: u8, ep_id: u8, cc: u8, remaining: u32) -> bool {
    for t in INTERRUPT_TARGETS.iter() {
        if t.slot_id.load(Ordering::Acquire) == slot_id && t.dci.load(Ordering::Relaxed) == ep_id {
            if t.cc_mask.load(Ordering::Relaxed) & (1 << cc) == 0 {
                return false;
            }
            t.remaining.store(remaining, Ordering::Relaxed);
            t.ready.store(true, Ordering::Release);
            return true;
        }
    }
    false
}

/// Read the Microframe Index Register (runtime offset 0x48): a free-running
/// 14-bit counter of 125us service periods.  Reserved for isochronous
/// frame-ID scheduling (`Frame ID` = bits 13:3); the current path uses
/// SIA=1 so MFINDEX is not consulted.
pub fn mfindex() -> u32 {
    let rt_va = XHCI_RT_VA.load(Ordering::Relaxed);
    if rt_va == 0 {
        return 0;
    }
    unsafe { core::ptr::read_volatile((rt_va + 0x48) as *const u32) }
}

static EVENT_RING_LOCK: Mutex<()> = Mutex::new(());

pub fn consume_pending_events() {
    let _guard = EVENT_RING_LOCK.lock();
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
                let slot_id = ((control >> 24) & 0xFF) as u8;
                usb_trace!({
                    SerialPort::puts("[xhci] evt: CMD cc=");
                    SerialPort::put_u64(cc as u64);
                    SerialPort::puts(" slot=");
                    SerialPort::put_u64(slot_id as u64);
                    SerialPort::puts(" cycle=");
                    SerialPort::put_u64((control & 1) as u64);
                    SerialPort::puts("\n");
                });
                let state =
                    (slot_id as u64) | ((cc as u64) << 8) | ((param as u64) << 16) | (1u64 << 63);
                LAST_CMD_STATE.store(state, Ordering::Release);
            }
            34 => {
                let param = unsafe { core::ptr::read_volatile(trb_va as *const u64) };
                let port_id = (param >> 24) & 0xFF;
                usb_trace!({
                    SerialPort::puts("[xhci] evt: PORT_CHANGE port=");
                    SerialPort::put_u64(port_id);
                    SerialPort::puts(" cycle=");
                    SerialPort::put_u64((control & 1) as u64);
                    SerialPort::puts("\n");
                });
                port_events_push(port_id as u8);
            }
            32 => {
                let status = unsafe { core::ptr::read_volatile((trb_va + 8) as *const u32) };
                let cc = (status >> 24) as u8;
                let remaining = status & 0xFFFFFF;
                let slot_id = ((control >> 24) & 0xFF) as u8;
                let ep_id = ((control >> 16) & 0x1F) as u8;
                usb_trace!({
                    SerialPort::puts("[xhci] evt: XFER cc=");
                    SerialPort::put_u64(cc as u64);
                    SerialPort::puts(" slot=");
                    SerialPort::put_u64(slot_id as u64);
                    SerialPort::puts(" ep=");
                    SerialPort::put_u64(ep_id as u64);
                    SerialPort::puts(" len=");
                    SerialPort::put_u64(remaining as u64);
                    SerialPort::puts("\n");
                });
                if route_to_interrupt_target(slot_id, ep_id, cc, remaining) {
                    // Consumed by a registered transfer target; leave
                    // LAST_TRANSFER_STATE untouched so wait_for_transfer
                    // never observes a foreign completion.
                } else {
                    let state = (slot_id as u64) << 48
                        | (ep_id as u64) << 40
                        | (cc as u64) << 32
                        | (remaining as u64)
                        | (1u64 << 63);
                    LAST_TRANSFER_STATE.store(state, Ordering::Release);
                }
            }
            37 => {
                usb_trace!({
                    SerialPort::puts("[xhci] evt: HOST_CTRL cycle=");
                    SerialPort::put_u64((control & 1) as u64);
                    SerialPort::puts("\n");
                });
            }
            _ => {
                usb_trace!({
                    SerialPort::puts("[xhci] evt: UNKNOWN type=");
                    SerialPort::put_u64(trb_type as u64);
                    SerialPort::puts(" cycle=");
                    SerialPort::put_u64((control & 1) as u64);
                    SerialPort::puts("\n");
                });
            }
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

/// Non-destructive peek at the latest command completion (does not clear
/// `LAST_CMD_STATE`).  For wait-loop predicates that must not consume the
/// event before the caller reads it.
pub fn peek_last_command_completion() -> Option<(u8, u8, u32)> {
    let state = LAST_CMD_STATE.load(Ordering::Acquire);
    if state & (1u64 << 63) != 0 {
        let slot = state as u8;
        let cc = (state >> 8) as u8;
        let param = (state >> 16) as u32;
        Some((slot, cc, param))
    } else {
        None
    }
}

pub fn last_transfer_completion() -> Option<(u8, u8, u8, u32)> {
    let state = LAST_TRANSFER_STATE.swap(0, Ordering::AcqRel);
    if state & (1u64 << 63) != 0 {
        let slot_id = (state >> 48) as u8;
        let ep_id = (state >> 40) as u8;
        let cc = (state >> 32) as u8;
        let remaining = state as u32;
        Some((slot_id, ep_id, cc, remaining))
    } else {
        None
    }
}

/// Non-destructive peek at the latest transfer completion (does not clear
/// `LAST_TRANSFER_STATE`).
pub fn peek_last_transfer_completion() -> Option<(u8, u8, u8, u32)> {
    let state = LAST_TRANSFER_STATE.load(Ordering::Acquire);
    if state & (1u64 << 63) != 0 {
        let slot_id = (state >> 48) as u8;
        let ep_id = (state >> 40) as u8;
        let cc = (state >> 32) as u8;
        let remaining = state as u32;
        Some((slot_id, ep_id, cc, remaining))
    } else {
        None
    }
}
