use super::memory::InputControlContext;

const SLOT_CTX_ENTRIES_SHIFT: u32 = 27;

const SLOT1_PORT_NUM_SHIFT: u32 = 16;
const SLOT1_SPEED_SHIFT: u32 = 24;

const EP_CERR_SHIFT: u32 = 1;
const EP_TYPE_SHIFT: u32 = 3;
const EP_MAX_PACKET_SHIFT: u32 = 16;
const EP_MAX_BURST_SHIFT: u32 = 8;
const EP_AVG_TRB_LENGTH_SHIFT: u32 = 0;
const EP_DCS: u64 = 1;

pub const EP_TYPE_CONTROL: u32 = 4;
pub const EP_TYPE_BULK_OUT: u32 = 2;
pub const EP_TYPE_BULK_IN: u32 = 6;
pub const EP_TYPE_INTERRUPT_OUT: u32 = 3;
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
}

fn init_slot_context(ctx: &mut [u32; 8], speed: u8, port_num: u8, context_entries: u8, address: u8) {
    ctx[0] = (context_entries as u32) << SLOT_CTX_ENTRIES_SHIFT
        | (address as u32) << 24;
    ctx[1] = (port_num as u32) << SLOT1_PORT_NUM_SHIFT
        | (speed as u32) << SLOT1_SPEED_SHIFT;
    for i in 2..8 {
        ctx[i] = 0;
    }
}

fn init_ep0_context(ctx: &mut [u32; 8], mps: u16, dequeue_phys: u64, cerr: u8, avg_trb_len: u16) {
    let ep_type = EP_TYPE_CONTROL;
    let dw0 = 0;
    ctx[0] = dw0;
    let dw1 = ((cerr as u32) & 0x3) << EP_CERR_SHIFT
        | (ep_type & 0x7) << EP_TYPE_SHIFT
        | (mps as u32) << EP_MAX_PACKET_SHIFT;
    ctx[1] = dw1;
    let dequeue = dequeue_phys | EP_DCS;
    ctx[2] = dequeue as u32;
    ctx[3] = (dequeue >> 32) as u32;
    ctx[4] = (avg_trb_len as u32) << EP_AVG_TRB_LENGTH_SHIFT;
    for i in 5..8 {
        ctx[i] = 0;
    }
}

fn init_ep_context(ctx: &mut [u32; 8], ep_type: u32, mps: u16, dequeue_phys: u64, cerr: u8, avg_trb_len: u16, max_burst: u8, _interval: u8) {
    ctx[0] = 0;
    let dw1 = ((cerr as u32) & 0x3) << EP_CERR_SHIFT
        | (ep_type & 0x7) << EP_TYPE_SHIFT
        | (mps as u32) << EP_MAX_PACKET_SHIFT
        | ((max_burst as u32) & 0xFF) << EP_MAX_BURST_SHIFT;
    ctx[1] = dw1;
    let dequeue = dequeue_phys | EP_DCS;
    ctx[2] = dequeue as u32;
    ctx[3] = (dequeue >> 32) as u32;
    ctx[4] = (avg_trb_len as u32) << EP_AVG_TRB_LENGTH_SHIFT;
    for i in 5..8 {
        ctx[i] = 0;
    }
}

pub fn init_icc_for_address_device(
    icc: &mut InputControlContext,
    speed: u8,
    port_num: u8,
    mps: u16,
    dequeue_phys: u64,
    address: u8,
) {
    icc.drop_flags = 0;
    icc.add_flags = 0x3;
    init_slot_context(&mut icc.slot_context, speed, port_num, 1, address);
    init_ep0_context(&mut icc.ep_contexts[0], mps, dequeue_phys, 3, 8);
}

pub fn init_icc_for_configure_endpoint(
    icc: &mut InputControlContext,
    speed: u8,
    port_num: u8,
    address: u8,
    ep0_mps: u16,
    ep0_dequeue_phys: u64,
    endpoints: &[EndpointConfig],
) {
    icc.drop_flags = 0;
    icc.add_flags = 0x3;
    let mut max_dci = 1u8;
    for ep in endpoints {
        if ep.dci > max_dci {
            max_dci = ep.dci;
        }
        icc.add_flags |= 1u32 << (ep.dci as u32);
    }
    init_slot_context(&mut icc.slot_context, speed, port_num, max_dci, address);
    init_ep0_context(&mut icc.ep_contexts[0], ep0_mps, ep0_dequeue_phys, 3, 8);
    for ep in endpoints {
        let ep_index = (ep.dci - 1) as usize;
        if ep_index < 31 {
            init_ep_context(
                &mut icc.ep_contexts[ep_index],
                ep.ep_type,
                ep.max_packet_size,
                ep.dequeue_phys,
                ep.cerr,
                ep.avg_trb_len,
                ep.max_burst,
                ep.interval,
            );
        }
    }
}
