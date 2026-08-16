//! Input Control Context builders.
//!
//! xHCI context entries are 32 bytes (CSZ=0) or 64 bytes (CSZ=1, HCCPARAMS1
//! bit 2).  All builders write through raw virtual addresses at the spec
//! offsets: the 32-byte Input Control context header at offset 0, the slot
//! context at offset 32, and each endpoint context at offset
//! 32 + context_index * ctx_size (context_index == DCI, EP0 == 1).

const SLOT_CTX_OFF: u64 = 0x20;

const SLOT_CTX_ENTRIES_SHIFT: u32 = 27;
const SLOT_SPEED_SHIFT: u32 = 20;

const EP_CERR_SHIFT: u32 = 1;
const EP_TYPE_SHIFT: u32 = 3;
const EP_MAX_PACKET_SHIFT: u32 = 16;
const EP_AVG_TRB_LENGTH_SHIFT: u32 = 0;
const EP_MAX_BURST_SHIFT: u32 = 8;
const EP_INTERVAL_SHIFT: u32 = 16;
const EP_MULT_SHIFT: u32 = 8;
const EP_ESIT_LO_SHIFT: u32 = 16;
const EP_DCS: u64 = 1;

pub const EP_TYPE_ISOCH_OUT: u32 = 1;
pub const EP_TYPE_BULK_OUT: u32 = 2;
pub const EP_TYPE_INTERRUPT_OUT: u32 = 3;
pub const EP_TYPE_CONTROL: u32 = 4;
pub const EP_TYPE_ISOCH_IN: u32 = 5;
pub const EP_TYPE_BULK_IN: u32 = 6;
pub const EP_TYPE_INTERRUPT_IN: u32 = 7;

pub struct EndpointConfig {
    pub dci: u8,
    pub ep_type: u32,
    pub max_packet_size: u16,
    pub dequeue_phys: u64,
    pub cerr: u8,
    pub avg_trb_len: u16,
    pub max_burst: u8,
    pub interval: u8,
    /// Isochronous (SuperSpeed) endpoint context Mult field — the number of
    /// packets per service interval (spec Table 6-8 dw0:8-9).  0 otherwise.
    pub mult: u8,
    /// Max ESIT Payload (dw4:16-31); non-zero for isochronous/interrupt
    /// endpoints, 0 for bulk/control.
    pub max_esit_payload: u16,
}

fn write32(va: u64, off: u64, val: u32) {
    unsafe { core::ptr::write_volatile((va + off) as *mut u32, val) }
}

fn ep_ctx_off(ctx_size: u8, context_index: u8) -> u64 {
    SLOT_CTX_OFF + (context_index as u64) * (ctx_size as u64)
}

fn init_slot_context(icc_va: u64, speed: u8, port_num: u8, context_entries: u8) {
    let base = icc_va + SLOT_CTX_OFF;
    // dw0: Context Entries | Speed; dw1: Root Hub Port Number.
    write32(
        base,
        0x00,
        ((context_entries as u32) & 0x1F) << SLOT_CTX_ENTRIES_SHIFT
            | ((speed as u32) & 0xF) << SLOT_SPEED_SHIFT,
    );
    write32(base, 0x04, (port_num as u32) << 16);
}

fn init_ep_context(
    icc_va: u64,
    ctx_size: u8,
    context_index: u8,
    ep_type: u32,
    mps: u16,
    dequeue_phys: u64,
    cerr: u8,
    avg_trb_len: u16,
    max_burst: u8,
    interval: u8,
    mult: u8,
    max_esit_payload: u16,
) {
    let base = icc_va + ep_ctx_off(ctx_size, context_index);
    // dw0 bits 23:16: Interval — the polling period as `125us * 2^Interval`
    // (spec Table 6-8).  Zero for bulk/control (never NAK).  dw0 bits 9:8:
    // Mult (SuperSpeed isochronous only; 0 below).
    write32(
        base,
        0x00,
        ((interval as u32) & 0xFF) << EP_INTERVAL_SHIFT | ((mult as u32) & 0x3) << EP_MULT_SHIFT,
    );
    // dw1: CErr | EP Type | Max Burst Size (spec Table 6-8, dw1:8-15) |
    // Max Packet Size.
    write32(
        base,
        0x04,
        ((cerr as u32) & 0x3) << EP_CERR_SHIFT
            | (ep_type & 0x7) << EP_TYPE_SHIFT
            | ((max_burst as u32) & 0xFF) << EP_MAX_BURST_SHIFT
            | (mps as u32) << EP_MAX_PACKET_SHIFT,
    );
    // dw2/dw3: Dequeue Pointer | DCS; dw4: Average TRB Length | Max ESIT
    // Payload Lo (spec Table 6-8).
    let dequeue = dequeue_phys | EP_DCS;
    write32(base, 0x08, dequeue as u32);
    write32(base, 0x0C, (dequeue >> 32) as u32);
    write32(
        base,
        0x10,
        (avg_trb_len as u32) << EP_AVG_TRB_LENGTH_SHIFT
            | ((max_esit_payload as u32) & 0xFFFF) << EP_ESIT_LO_SHIFT,
    );
}

/// Build an Input Control Context for an Address Device command: slot +
/// EP0 with an unknown (speed-default) MaxPacketSize.  EP0 uses DCI 1.
pub fn init_icc_for_address_device(
    icc_va: u64,
    ctx_size: u8,
    speed: u8,
    port_num: u8,
    mps: u16,
    dequeue_phys: u64,
) {
    // Add Context Flags: slot (bit 0) + EP0 (bit 1).
    write32(icc_va, 0x00, 0);
    write32(icc_va, 0x04, 0x3);
    init_slot_context(icc_va, speed, port_num, 1);
    init_ep_context(
        icc_va,
        ctx_size,
        1,
        EP_TYPE_CONTROL,
        mps,
        dequeue_phys,
        3,
        8,
        0,
        0,
        0,
        0,
    );
}

/// Build an Input Control Context for an Evaluate Context command that
/// updates only EP0's MaxPacketSize once the device descriptor is known.
pub fn init_icc_for_evaluate_ep0(
    icc_va: u64,
    ctx_size: u8,
    speed: u8,
    port_num: u8,
    mps: u16,
    dequeue_phys: u64,
) {
    // Add Context Flags: slot (bit 0) + EP0 (bit 1).
    write32(icc_va, 0x00, 0);
    write32(icc_va, 0x04, 0x3);
    init_slot_context(icc_va, speed, port_num, 1);
    init_ep_context(
        icc_va,
        ctx_size,
        1,
        EP_TYPE_CONTROL,
        mps,
        dequeue_phys,
        3,
        8,
        0,
        0,
        0,
        0,
    );
}

/// Build an Input Control Context for a Configure Endpoint command that
/// configures the given non-default endpoints (EP0 is never included).
pub fn init_icc_for_configure_endpoint(
    icc_va: u64,
    ctx_size: u8,
    speed: u8,
    port_num: u8,
    endpoints: &[EndpointConfig],
) {
    let mut add_flags = 0x1u32;
    let mut max_dci = 1u8;
    for ep in endpoints {
        if ep.dci > max_dci {
            max_dci = ep.dci;
        }
        add_flags |= 1u32 << (ep.dci as u32);
    }
    write32(icc_va, 0x00, 0);
    write32(icc_va, 0x04, add_flags);
    init_slot_context(icc_va, speed, port_num, max_dci);
    for ep in endpoints {
        if ep.dci >= 2 && ep.dci <= 31 {
            init_ep_context(
                icc_va,
                ctx_size,
                ep.dci,
                ep.ep_type,
                ep.max_packet_size,
                ep.dequeue_phys,
                ep.cerr,
                ep.avg_trb_len,
                ep.max_burst,
                ep.interval,
                ep.mult,
                ep.max_esit_payload,
            );
        }
    }
}
