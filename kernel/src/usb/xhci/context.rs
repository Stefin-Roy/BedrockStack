use super::memory::InputControlContext;

const SLOT_CTX_ENTRIES_SHIFT: u32 = 27;

const SLOT1_PORT_NUM_SHIFT: u32 = 16;
const SLOT1_SPEED_SHIFT: u32 = 24;

const EP_STATE_DISABLED: u32 = 0;
const EP_CERR_SHIFT: u32 = 1;
const EP_TYPE_SHIFT: u32 = 3;
const EP_TYPE_CONTROL: u32 = 0;
const EP_MAX_PACKET_SHIFT: u32 = 16;
const EP_AVG_TRB_LENGTH_SHIFT: u32 = 0;
const EP_DCS: u64 = 1;

fn init_slot_context(ctx: &mut [u32; 8], speed: u8, port_num: u8, context_entries: u8) {
    ctx[0] = (context_entries as u32) << SLOT_CTX_ENTRIES_SHIFT;
    ctx[1] = (port_num as u32) << SLOT1_PORT_NUM_SHIFT
        | (speed as u32) << SLOT1_SPEED_SHIFT;
    for i in 2..8 {
        ctx[i] = 0;
    }
}

fn init_ep0_context(ctx: &mut [u32; 8], mps: u16, dequeue_phys: u64, cerr: u8, avg_trb_len: u16) {
    let ep_state = EP_STATE_DISABLED;
    let ep_type = EP_TYPE_CONTROL;
    let dw0 = ep_state | (0u32) << 8 | (0u32) << 16 | (0u32) << 24;
    ctx[0] = dw0;

    let dw1 = ((cerr as u32) & 0x3) << EP_CERR_SHIFT
        | (ep_type & 0x7) << EP_TYPE_SHIFT
        | (mps as u32) << EP_MAX_PACKET_SHIFT;
    ctx[1] = dw1;

    let dequeue = dequeue_phys | EP_DCS;
    ctx[2] = dequeue as u32;
    ctx[3] = (dequeue >> 32) as u32;

    ctx[4] = (avg_trb_len as u32) << EP_AVG_TRB_LENGTH_SHIFT | 0u32 << 16;

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
) {
    icc.drop_flags = 0;
    icc.add_flags = 0x3;

    init_slot_context(&mut icc.slot_context, speed, port_num, 1);

    init_ep0_context(&mut icc.ep_contexts[0], mps, dequeue_phys, 3, 8);
}
