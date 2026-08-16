use crate::drivers::serial::SerialPort;
use crate::services::dma::DmaAllocator;
use crate::usb::usb;
use crate::usb::usb::SetupPacket;
use crate::usb::usb::descriptors::{
    ConfigDescriptor, EndpointDescriptor, InterfaceDescriptor, SsEndpointCompanionDescriptor,
    SsIsochEpCompanionDescriptor,
};
use crate::usb::xhci::command;
use crate::usb::xhci::context::{self, EndpointConfig};
use crate::usb::xhci::event;
use crate::usb::xhci::memory::{self, TrbRing};
use alloc::vec::Vec;

/// A USB interface parsed from the configuration descriptor.  Endpoints carry
/// the xHCI endpoint-context fields so the Configure Endpoint command can be
/// built directly from them.  Only records worth configuring are kept for
/// each alternate setting; non-zero settings are retained so the isochronous
/// transport path can select them via SET_INTERFACE.
#[derive(Clone)]
pub struct UsbInterface {
    pub iface_num: u8,
    pub alt_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<UsbEndpoint>,
}

/// One non-default endpoint of an interface.
/// `ep_type` is the USB transfer type (`usb::EP_TYPE_BULK`/`EP_TYPE_INTERRUPT`/
/// `EP_TYPE_ISOCH`); `interval` is the pre-converted xHCI endpoint-context
/// Interval value (spec Table 6-12), 0 for bulk.
#[derive(Clone, Copy)]
pub struct UsbEndpoint {
    pub dci: u8,
    pub ep_type: u8,
    pub mps: u16,
    pub interval: u8,
    /// Max Burst Size — HS isoch/interrupt `wMaxPacketSize` bits 12:11, SS
    /// `bMaxBurst` from the USB 3.0 EP Companion descriptor, 0 for FS/LS.
    pub max_burst: u8,
    /// Endpoint-context Mult (SuperSpeed isochronous only); 0 here.
    pub mult: u8,
    /// Max ESIT Payload (xHCI §4.14.2): FS = mps, HS = mps × (burst+1),
    /// SS = companion wBytesPerInterval.  0 for bulk.
    pub max_esit_payload: u16,
}

pub struct DeviceSlot {
    pub slot_id: u8,
    pub port_num: u8,
    pub speed: u8,
    pub ctx_size: u8,
    pub mps: u16,
    pub icc_phys: u64,
    pub icc_va: u64,
    pub ep0_ring: TrbRing,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub ep_rings: Vec<(u8, TrbRing)>,
    pub config_value: u8,
    pub interfaces: Vec<UsbInterface>,
}

impl DeviceSlot {
    pub fn new(
        slot_id: u8,
        port_num: u8,
        speed: u8,
        ctx_size: u8,
        mps: u16,
        icc_phys: u64,
        icc_va: u64,
        ep0_ring: TrbRing,
        address: u8,
    ) -> Self {
        DeviceSlot {
            slot_id,
            port_num,
            speed,
            ctx_size,
            mps,
            icc_phys,
            icc_va,
            ep0_ring,
            address,
            vendor_id: 0,
            product_id: 0,
            ep_rings: Vec::new(),
            config_value: 0,
            interfaces: Vec::new(),
        }
    }
}

/// Translate a USB `bInterval` into the xHCI endpoint-context Interval value
/// (spec Table 6-12).  The context value is an exponent: service period is
/// `125us * 2^n`.  `ep_type` distinguishes interrupt (frame-based on FS/LS)
/// from isochronous (exponent-based in 1 ms units on FS/LS) encoding.
fn usb_interval_to_context(speed: u8, ep_type: u8, binterval: u8) -> u8 {
    if binterval == 0 {
        return 0;
    }
    match (speed, ep_type) {
        // FS isoch bInterval is a base-2 exponent of 1 ms frames:
        // `2^(bInterval-1) ms`.  Valid context range is 3-18.  HS/SS isoch
        // uses the `2^(b-1) * 125us` formula (the `_` arm below).
        (usb::SPEED_FS | usb::SPEED_LS, usb::EP_TYPE_ISOCH) => {
            (binterval as u32 + 2).clamp(3, 18) as u8
        }
        // FS/LS interrupt bInterval is in 1 ms frames.  The service interval
        // (in 125us microframes) must be a power of two; round the frame
        // count up and take log2.  Valid context range is 3-10.
        (usb::SPEED_FS | usb::SPEED_LS, _) => {
            let microframes = binterval as u32 * 8;
            let pow2 = microframes.next_power_of_two();
            (pow2.trailing_zeros() as u8).clamp(3, 10)
        }
        // HS/SS bInterval is already `2^(b-1) * 125us`.  Valid range 0-15.
        _ => (binterval.saturating_sub(1)).min(15),
    }
}

fn wait_for_transfer(slot_id: u8, ep_id: u8) -> Result<u8, &'static str> {
    wait_for_transfer_timeout(slot_id, ep_id, 5_000_000_000)
}

fn wait_for_transfer_timeout(slot_id: u8, ep_id: u8, timeout_ns: u64) -> Result<u8, &'static str> {
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + timeout_ns;
    let completed = wait_until_cond(deadline, &|| {
        event::consume_pending_events();
        event::peek_last_transfer_completion()
            .map(|(sid, eid, _cc, _remaining)| sid == slot_id && eid == ep_id)
            .unwrap_or(false)
    });
    if !completed {
        return Err("transfer timeout");
    }
    let (_sid, _eid, cc, _remaining) = event::last_transfer_completion().unwrap();
    // Success (1), Short Packet (13), and the isochronous pair Isoch Buffer
    // Overrun (10) / Underrun (11) all count as an executed transfer — the
    // last two are the normal outcome when a device sends more/fewer bytes
    // than the ESIT allowed.
    if cc == 1 || cc == 13 || cc == 10 || cc == 11 {
        Ok(cc)
    } else {
        Err("transfer failed")
    }
}

/// Submit a full control transfer (Setup + optional Data + Status) on an
/// endpoint-0 transfer ring and wait for its completion.  Ringing the
/// doorbell, flushing, and the TRB sequence follow xHCI §4.11.2.2:
/// - a Setup Stage TRB carrying the 8-byte setup data (immediate, IDT),
/// - a Data Stage TRB only when `data_len > 0` (DIR_IN if `dir_in`),
/// - a Status Stage TRB whose direction is flipped relative to the data stage.
///
/// `submit_control` is the `DeviceSlot`-based wrapper; class drivers with
/// only the ring (e.g. HID issuing `SET_PROTOCOL` from `init_interface`)
/// call this directly.
pub fn submit_control_transfer(
    ep0_ring: &mut TrbRing,
    doorbell_va: u64,
    slot_id: u8,
    setup: &SetupPacket,
    data_phys: u64,
    data_len: u16,
    dir_in: bool,
) -> Result<u32, &'static str> {
    let setup_raw: [u8; 8] = [
        setup.bm_request_type,
        setup.b_request,
        setup.w_value as u8,
        (setup.w_value >> 8) as u8,
        setup.w_index as u8,
        (setup.w_index >> 8) as u8,
        setup.w_length as u8,
        (setup.w_length >> 8) as u8,
    ];
    let trt: u32 = if data_len == 0 {
        0
    } else if dir_in {
        3
    } else {
        2
    };

    ep0_ring.enqueue(&memory::make_setup_stage_trb(&setup_raw, trt));

    if data_len > 0 {
        ep0_ring.enqueue(&memory::make_data_stage_trb(
            data_phys,
            data_len as u32,
            dir_in,
        ));
    }

    ep0_ring.enqueue(&memory::make_status_stage_trb(dir_in));

    ep0_ring.flush();
    command::ring_doorbell(doorbell_va, slot_id, 1);

    let cc = wait_for_transfer(slot_id, 1)?;
    Ok(cc as u32)
}

fn submit_control(
    slot: &mut DeviceSlot,
    doorbell_va: u64,
    setup: &SetupPacket,
    data_phys: u64,
    data_len: u16,
    dir_in: bool,
) -> Result<u32, &'static str> {
    submit_control_transfer(
        &mut slot.ep0_ring,
        doorbell_va,
        slot.slot_id,
        setup,
        data_phys,
        data_len,
        dir_in,
    )
}

pub fn get_device_descriptor(
    slot: &mut DeviceSlot,
    doorbell_va: u64,
    data_phys: u64,
    data_va: u64,
) -> Result<(), &'static str> {
    let setup = SetupPacket::get_descriptor(usb::DESC_DEVICE, 0, 0, 18);
    submit_control(slot, doorbell_va, &setup, data_phys, 18, true)?;

    let desc = unsafe { &*(data_va as *const [u8; 18]) };
    slot.vendor_id = u16::from_le_bytes([desc[8], desc[9]]);
    slot.product_id = u16::from_le_bytes([desc[10], desc[11]]);

    Ok(())
}

pub fn get_config_descriptor_full(
    slot: &mut DeviceSlot,
    doorbell_va: u64,
    data_phys: u64,
    data_va: u64,
) -> Result<(), &'static str> {
    let setup_hdr = SetupPacket::get_descriptor(usb::DESC_CONFIG, 0, 0, 9);
    submit_control(slot, doorbell_va, &setup_hdr, data_phys, 9, true)?;

    let buf = unsafe { core::slice::from_raw_parts(data_va as *const u8, 9) };
    let cfg = ConfigDescriptor::parse(buf).ok_or("bad config desc hdr")?;
    let total_len = cfg.total_length() as usize;

    if total_len > 4096 {
        return Err("config desc too large");
    }

    let setup_full = SetupPacket::get_descriptor(usb::DESC_CONFIG, 0, 0, total_len as u16);
    submit_control(
        slot,
        doorbell_va,
        &setup_full,
        data_phys,
        total_len as u16,
        true,
    )?;

    let cfg_buf = unsafe { core::slice::from_raw_parts(data_va as *const u8, total_len) };
    let limit = cfg_buf.len();

    if cfg!(feature = "usb_trace") {
        SerialPort::puts("[xhci]  config desc slot=");
        SerialPort::put_u64(slot.slot_id as u64);
        SerialPort::puts(" (");
        SerialPort::put_u64(total_len as u64);
        SerialPort::puts(" bytes)\n");
    }

    // Rebuild the interface table from scratch so this function is
    // idempotent even if called twice on the same slot.
    slot.interfaces.clear();

    let mut cur_iface_num: u8 = 0;
    let mut cur_alt_setting: u8 = 0;
    // USB 3.x Endpoint Companion descriptors immediately follow the endpoint
    // they describe; fold their bMaxBurst / wBytesPerInterval into the next
    // recorded endpoint.
    let mut pend_max_burst: u8 = 0;
    let mut pend_esit: u16 = 0;

    let mut offset = 0usize;
    while offset < limit {
        if offset + 2 > limit {
            break;
        }
        let len = cfg_buf[offset] as usize;
        let desc_type = cfg_buf[offset + 1];
        if len < 2 || offset + len > limit {
            break;
        }
        match desc_type {
            usb::DESC_CONFIG => {
                if let Some(cfg_desc) = ConfigDescriptor::parse(&cfg_buf[offset..]) {
                    slot.config_value = cfg_desc.configuration_value();
                }
            }
            usb::DESC_INTERFACE => {
                if let Some(iface) = InterfaceDescriptor::parse(&cfg_buf[offset..]) {
                    cur_iface_num = iface.interface_number();
                    cur_alt_setting = iface.alternate_setting();
                    // Record the interface at any alternate setting.  Alt 0 is
                    // what class drivers bind; non-zero alts are kept so the
                    // isochronous transport path can configure their endpoints
                    // (UAC1 streaming exposes them only at alt >= 1).
                    slot.interfaces.push(UsbInterface {
                        iface_num: cur_iface_num,
                        alt_setting: cur_alt_setting,
                        class: iface.class(),
                        subclass: iface.subclass(),
                        protocol: iface.protocol(),
                        endpoints: Vec::new(),
                    });
                    if cfg!(feature = "usb_trace") {
                        SerialPort::puts("[xhci]    iface ");
                        SerialPort::put_u64(iface.interface_number() as u64);
                        SerialPort::puts(" alt=");
                        SerialPort::put_u64(iface.alternate_setting() as u64);
                        SerialPort::puts(" class=0x");
                        SerialPort::put_hex(iface.class() as u64);
                        SerialPort::puts(" subclass=0x");
                        SerialPort::put_hex(iface.subclass() as u64);
                        SerialPort::puts(" protocol=0x");
                        SerialPort::put_hex(iface.protocol() as u64);
                        SerialPort::puts("\n");
                    }
                }
            }
            usb::DESC_ENDPOINT => {
                if let Some(ep) = EndpointDescriptor::parse(&cfg_buf[offset..]) {
                    let ep_num = ep.endpoint_number();
                    let is_in = ep.is_in();
                    let dci: u8 = (ep_num * 2) + if is_in { 1 } else { 0 };
                    let ep_type = ep.transfer_type();
                    // Record every non-default transfer type.  Isochronous
                    // endpoints are kept at any alternate setting; bulk and
                    // interrupt only at alt 0 (the binding target).
                    let record = match ep_type {
                        usb::EP_TYPE_BULK | usb::EP_TYPE_INTERRUPT => cur_alt_setting == 0,
                        usb::EP_TYPE_ISOCH => true,
                        _ => false,
                    };
                    // Skip the default control pipe (EP0) and out-of-range DCIs.
                    if record && ep_num != 0 && ep_num <= 15 {
                        if let Some(iface) = slot.interfaces.iter_mut().find(|i| {
                            i.iface_num == cur_iface_num && i.alt_setting == cur_alt_setting
                        }) {
                            let usb_ep = UsbEndpoint {
                                dci,
                                ep_type,
                                mps: ep.max_packet_size(),
                                interval: if ep_type == usb::EP_TYPE_BULK {
                                    0
                                } else {
                                    usb_interval_to_context(slot.speed, ep_type, ep.interval())
                                },
                                max_burst: if ep_type == usb::EP_TYPE_BULK {
                                    0
                                } else {
                                    endpoint_max_burst(slot.speed, ep_type, ep, pend_max_burst)
                                },
                                mult: 0,
                                max_esit_payload: if ep_type == usb::EP_TYPE_BULK {
                                    0
                                } else {
                                    endpoint_max_esit(slot.speed, ep_type, ep, pend_esit)
                                },
                            };
                            iface.endpoints.push(usb_ep);
                            if cfg!(feature = "usb_trace") {
                                SerialPort::puts("[xhci]      ");
                                SerialPort::puts(match (ep_type, is_in) {
                                    (usb::EP_TYPE_BULK, true) => "bulk IN ",
                                    (usb::EP_TYPE_BULK, false) => "bulk OUT",
                                    (usb::EP_TYPE_INTERRUPT, true) => "intr IN ",
                                    (usb::EP_TYPE_INTERRUPT, false) => "intr OUT",
                                    (usb::EP_TYPE_ISOCH, true) => "isoch IN ",
                                    (usb::EP_TYPE_ISOCH, false) => "isoch OUT",
                                    _ => "unknown ",
                                });
                                SerialPort::puts(" dci=");
                                SerialPort::put_u64(dci as u64);
                                SerialPort::puts(" mps=");
                                SerialPort::put_u64(ep.max_packet_size() as u64);
                                SerialPort::puts(" esit=");
                                SerialPort::put_u64(usb_ep.max_esit_payload as u64);
                                SerialPort::puts("\n");
                            }
                        }
                    }
                    // The companion (if any) applied only to the endpoint it
                    // followed; clear it regardless.
                    pend_max_burst = 0;
                    pend_esit = 0;
                }
            }
            usb::DESC_SS_EP_COMPANION => {
                if let Some(c) = SsEndpointCompanionDescriptor::parse(&cfg_buf[offset..]) {
                    pend_max_burst = c.max_burst();
                    pend_esit = c.bytes_per_interval();
                }
            }
            usb::DESC_SS_ISOCH_EP_COMPANION => {
                if let Some(c) = SsIsochEpCompanionDescriptor::parse(&cfg_buf[offset..]) {
                    pend_esit = c.bytes_per_interval().min(0xFFFF) as u16;
                }
            }
            _ => {}
        }
        offset += len;
    }

    // Drop non-zero alternate settings that carried no recordable endpoints
    // (a UAC streaming alt 1 with no isoch endpoints would otherwise pollute
    // the list the drivers and Configure Endpoint scan).
    slot.interfaces
        .retain(|i| i.alt_setting == 0 || !i.endpoints.is_empty());

    if cfg!(feature = "usb_trace") {
        SerialPort::puts("[xhci]  ");
        SerialPort::put_u64(slot.interfaces.len() as u64);
        SerialPort::puts(" interface(s) recorded\n");
    }

    Ok(())
}

/// Max Burst Size for a recorded non-bulk endpoint (spec Table 6-8 dw1:8-15):
/// HS isoch/interrupt = `wMaxPacketSize` bits 12:11, SS = companion
/// `bMaxBurst`, FS/LS = 0.
fn endpoint_max_burst(speed: u8, _ep_type: u8, ep: &EndpointDescriptor, ss_burst: u8) -> u8 {
    match speed {
        usb::SPEED_HS => ep.hs_burst(),
        usb::SPEED_SS => ss_burst,
        _ => 0,
    }
}

/// Max ESIT Payload for a recorded non-bulk endpoint (xHCI §4.14.2): FS = mps,
/// HS = mps × (burst + 1), SS = companion `wBytesPerInterval`.
fn endpoint_max_esit(speed: u8, ep_type: u8, ep: &EndpointDescriptor, ss_esit: u16) -> u16 {
    match (speed, ep_type) {
        (usb::SPEED_FS, usb::EP_TYPE_ISOCH) => ep.max_packet_size(),
        (usb::SPEED_HS, usb::EP_TYPE_ISOCH) => ep
            .max_packet_size()
            .saturating_mul(ep.hs_burst() as u16 + 1),
        (usb::SPEED_SS, usb::EP_TYPE_ISOCH) => {
            if ss_esit != 0 {
                ss_esit
            } else {
                ep.max_packet_size()
            }
        }
        (usb::SPEED_FS | usb::SPEED_LS, usb::EP_TYPE_INTERRUPT) => ep.max_packet_size(),
        (usb::SPEED_HS, usb::EP_TYPE_INTERRUPT) => ep
            .max_packet_size()
            .saturating_mul(ep.hs_burst() as u16 + 1),
        (usb::SPEED_SS, usb::EP_TYPE_INTERRUPT) => {
            if ss_esit != 0 {
                ss_esit
            } else {
                ep.max_packet_size()
            }
        }
        _ => 0,
    }
}

/// Map a USB transfer type + direction to the xHCI endpoint-context type
/// (spec §6.2.3 Table 6-4: types are per-direction; DCI parity encodes it,
/// even = OUT, odd = IN).
fn context_ep_type(ep_type: u8, dci: u8) -> u32 {
    match (ep_type, dci & 1) {
        (usb::EP_TYPE_ISOCH, 0) => context::EP_TYPE_ISOCH_OUT,
        (usb::EP_TYPE_ISOCH, _) => context::EP_TYPE_ISOCH_IN,
        (usb::EP_TYPE_BULK, 0) => context::EP_TYPE_BULK_OUT,
        (usb::EP_TYPE_BULK, _) => context::EP_TYPE_BULK_IN,
        (usb::EP_TYPE_INTERRUPT, 0) => context::EP_TYPE_INTERRUPT_OUT,
        (usb::EP_TYPE_INTERRUPT, _) => context::EP_TYPE_INTERRUPT_IN,
        _ => context::EP_TYPE_CONTROL,
    }
}

/// Configure the given interfaces' endpoints on the xHC.
///
/// `iface_indices` indexes into `slot.interfaces`; every endpoint of every
/// listed interface gets a transfer ring and an endpoint-context entry, all
/// applied by a single Configure Endpoint command (spec §4.3.5).  Per the
/// xHCI spec, SET_CONFIGURATION to the USB device MUST precede the Configure
/// Endpoint command to the xHC.
pub fn configure_device(
    slot: &mut DeviceSlot,
    cmd_ring: &mut TrbRing,
    doorbell_va: u64,
    dma: &dyn DmaAllocator,
    iface_indices: &[usize],
) -> Result<(), &'static str> {
    if slot.config_value != 0 {
        let setup = SetupPacket::set_configuration(slot.config_value);
        submit_control(slot, doorbell_va, &setup, 0, 0, false)?;
    }

    let icc_va = slot.icc_va;
    let ctx_size = slot.ctx_size;
    unsafe { core::ptr::write_bytes(icc_va as *mut u8, 0, 4096) };

    let mut ring_pairs: Vec<(u8, TrbRing)> = Vec::new();
    let mut endpoints: Vec<EndpointConfig> = Vec::new();

    // One transfer ring + endpoint context per endpoint DCI across the
    // matched interfaces.  Deduplicate by DCI in case two matched interfaces
    // share an endpoint (malformed but harmless).
    for &idx in iface_indices {
        let iface = &slot.interfaces[idx];
        for ep in &iface.endpoints {
            if ring_pairs.iter().any(|(d, _)| *d == ep.dci) {
                continue;
            }
            let ring = TrbRing::new(dma, 4096)?;
            endpoints.push(EndpointConfig {
                dci: ep.dci,
                ep_type: context_ep_type(ep.ep_type, ep.dci),
                max_packet_size: ep.mps,
                dequeue_phys: ring.phys,
                // Isochronous endpoints report no transaction errors, so CErr
                // must be 0 (xHCI §6.2.3.2); everything else uses the
                // default 3 retries.
                cerr: if ep.ep_type == usb::EP_TYPE_ISOCH {
                    0
                } else {
                    3
                },
                avg_trb_len: if ep.ep_type == usb::EP_TYPE_BULK {
                    3072
                } else {
                    ep.mps.max(8)
                },
                max_burst: ep.max_burst,
                interval: ep.interval,
                mult: ep.mult,
                max_esit_payload: ep.max_esit_payload,
            });
            ring_pairs.push((ep.dci, ring));
        }
    }

    // Only push to slot.ep_rings once all allocations succeeded.
    slot.ep_rings.extend(ring_pairs);

    context::init_icc_for_configure_endpoint(
        icc_va,
        ctx_size,
        slot.speed,
        slot.port_num,
        &endpoints,
    );

    command::submit_configure_endpoint(cmd_ring, doorbell_va, slot.icc_phys, slot.slot_id, false)?;

    SerialPort::puts("[xhci]  configured slot=");
    SerialPort::put_u64(slot.slot_id as u64);
    SerialPort::puts(" ifaces=");
    SerialPort::put_u64(iface_indices.len() as u64);
    SerialPort::puts(" eps=");
    SerialPort::put_u64(endpoints.len() as u64);
    for (dci, _) in &slot.ep_rings {
        SerialPort::puts(" dci=");
        SerialPort::put_u64(*dci as u64);
    }
    SerialPort::puts("\n");

    Ok(())
}

pub fn submit_bulk(
    ring: &mut TrbRing,
    doorbell_va: u64,
    slot_id: u8,
    dci: u8,
    data_phys: u64,
    data_len: u32,
) -> Result<(), &'static str> {
    if data_len > 65536 {
        return Err("bulk xfer exceeds 64 KiB per TRB");
    }
    let trb = memory::make_normal_trb(data_phys, data_len);
    ring.enqueue(&trb);
    ring.flush();
    command::ring_doorbell(doorbell_va, slot_id, dci);
    wait_for_transfer(slot_id, dci)?;
    Ok(())
}

/// Submit one interrupt-IN read and wait for its completion.  The caller
/// chooses the timeout; periodic polling paths use a short one.
pub fn submit_interrupt(
    ring: &mut TrbRing,
    doorbell_va: u64,
    slot_id: u8,
    dci: u8,
    data_phys: u64,
    data_len: u32,
    timeout_ns: u64,
) -> Result<(), &'static str> {
    let trb = memory::make_normal_trb(data_phys, data_len);
    ring.enqueue(&trb);
    ring.flush();
    command::ring_doorbell(doorbell_va, slot_id, dci);
    wait_for_transfer_timeout(slot_id, dci, timeout_ns)?;
    Ok(())
}

/// Submit a single isochronous transfer (one TRB covering one service
/// interval) and wait for its completion.  `frame_id` and `sia` follow the
/// isochronous TRB (Tables 6-32/33/34); the current callers use SIA=1 with
/// `frame_id=0`, deferring MFINDEX-based frame scheduling (follow-up).
pub fn submit_isoch(
    ring: &mut TrbRing,
    doorbell_va: u64,
    slot_id: u8,
    dci: u8,
    data_phys: u64,
    data_len: u32,
    timeout_ns: u64,
) -> Result<u8, &'static str> {
    let trb = memory::make_isoch_trb(data_phys, data_len, 0, true, false, 0, true);
    ring.enqueue(&trb);
    ring.flush();
    command::ring_doorbell(doorbell_va, slot_id, dci);
    let cc = wait_for_transfer_timeout(slot_id, dci, timeout_ns)?;
    Ok(cc)
}

/// Submit a single isochronous transfer without waiting.  Prefer this when
/// completions are routed to a registered transfer target; otherwise the
/// enqueued TRB's completion lands in the shared `LAST_TRANSFER_STATE` slot.
pub fn enqueue_isoch(
    ring: &mut TrbRing,
    doorbell_va: u64,
    slot_id: u8,
    dci: u8,
    data_phys: u64,
    data_len: u32,
) {
    let trb = memory::make_isoch_trb(data_phys, data_len, 0, true, false, 0, true);
    ring.enqueue(&trb);
    ring.flush();
    command::ring_doorbell(doorbell_va, slot_id, dci);
}

pub fn find_ep_ring(slot: &DeviceSlot, dci: u8) -> Option<&TrbRing> {
    if dci == 1 {
        return Some(&slot.ep0_ring);
    }
    slot.ep_rings
        .iter()
        .find(|(d, _)| *d == dci)
        .map(|(_, r)| r)
}

pub fn find_ep_ring_mut(slot: &mut DeviceSlot, dci: u8) -> Option<&mut TrbRing> {
    if dci == 1 {
        return Some(&mut slot.ep0_ring);
    }
    slot.ep_rings
        .iter_mut()
        .find(|(d, _)| *d == dci)
        .map(|(_, r)| r)
}

pub struct DeviceSlotManager {
    pub slots: Vec<DeviceSlot>,
    ctx_size: u8,
    max_slots: u8,
    next_address: u8,
}

impl DeviceSlotManager {
    pub fn new(ctx_size: u8, max_slots: u8) -> Self {
        DeviceSlotManager {
            slots: Vec::new(),
            ctx_size,
            max_slots,
            next_address: 1,
        }
    }

    pub fn enumerate_port(
        &mut self,
        cmd_ring: &mut TrbRing,
        doorbell_va: u64,
        dma: &dyn DmaAllocator,
        port_num: u8,
        speed: u8,
    ) -> Result<(), &'static str> {
        if self.slots.len() >= self.max_slots as usize {
            return Err("slot limit reached");
        }
        SerialPort::puts("[xhci] enumerate port ");
        SerialPort::put_u64(port_num as u64);
        SerialPort::puts(" speed=");
        SerialPort::put_u64(speed as u64);
        SerialPort::puts("\n");

        let slot_id = command::submit_enable_slot(cmd_ring, doorbell_va)?;

        let icc_buf = dma.alloc_page().ok_or("OOM for ICC")?;
        let desc_buf = dma.alloc_page().ok_or("OOM for desc")?;
        let ep0_ring = TrbRing::new(dma, 4096)?;

        let bsr = speed == usb::SPEED_FS;

        // Phase 1: Address Device.  For Full-Speed, use BSR (MPS=8, address=0)
        // per USB 2.0 §9.3.1.  For other speeds, use known MPS and the real
        // address.
        let (mps, address) = if bsr {
            (8u16, 0u8)
        } else {
            let mps = match speed {
                usb::SPEED_LS => 8,
                usb::SPEED_HS => 64,
                usb::SPEED_SS => 512,
                _ => 64,
            };
            (mps, self.next_address)
        };

        unsafe { core::ptr::write_bytes(icc_buf.virt as *mut u8, 0, 4096) };
        context::init_icc_for_address_device(
            icc_buf.virt,
            self.ctx_size,
            speed,
            port_num,
            mps,
            ep0_ring.phys,
        );
        command::submit_address_device(cmd_ring, doorbell_va, icc_buf.phys, slot_id, bsr)?;

        let mut slot = DeviceSlot::new(
            slot_id,
            port_num,
            speed,
            self.ctx_size,
            mps,
            icc_buf.phys,
            icc_buf.virt,
            ep0_ring,
            address,
        );

        if bsr {
            // Read first 8 bytes of device descriptor to discover bMaxPacketSize0
            // (USB 2.0 §9.4.3).  Reading 18 bytes with unknown MPS is unsafe — the
            // xHC may mishandle the split transfer on some implementations.
            let setup8 = SetupPacket::get_descriptor(usb::DESC_DEVICE, 0, 0, 8);
            submit_control(&mut slot, doorbell_va, &setup8, desc_buf.phys, 8, true)?;
            let desc_mps_raw =
                unsafe { core::ptr::read_volatile((desc_buf.virt + 7) as *const u8) };
            let real_mps = if desc_mps_raw < 8 {
                8
            } else {
                desc_mps_raw as u16
            };

            // Re-address with correct MPS and real device address (BSR=0).
            unsafe { core::ptr::write_bytes(icc_buf.virt as *mut u8, 0, 4096) };
            context::init_icc_for_address_device(
                icc_buf.virt,
                self.ctx_size,
                speed,
                port_num,
                real_mps,
                slot.ep0_ring.phys,
            );
            command::submit_address_device(cmd_ring, doorbell_va, icc_buf.phys, slot_id, false)?;
            slot.address = self.next_address;
            slot.mps = real_mps;

            // Read full 18-byte descriptor for vendor/product IDs.
            get_device_descriptor(&mut slot, doorbell_va, desc_buf.phys, desc_buf.virt)?;
        } else {
            // Non-BSR path: MPS is known from speed table, read full descriptor.
            get_device_descriptor(&mut slot, doorbell_va, desc_buf.phys, desc_buf.virt)?;
        }

        // EP0 MaxPacketSize0 from the device descriptor may exceed the
        // speed-table default (e.g. full-speed bMaxPacketSize0 = 64).
        // Propagate the real value to the xHC via Evaluate Context.
        let desc_mps = unsafe { core::ptr::read_volatile((desc_buf.virt + 7) as *const u8) };
        let real_mps = if desc_mps < 8 { 8 } else { desc_mps as u16 };
        if real_mps != slot.mps {
            unsafe { core::ptr::write_bytes(icc_buf.virt as *mut u8, 0, 4096) };
            context::init_icc_for_evaluate_ep0(
                icc_buf.virt,
                self.ctx_size,
                speed,
                port_num,
                real_mps,
                slot.ep0_ring.phys,
            );
            command::submit_evaluate_context(cmd_ring, doorbell_va, icc_buf.phys, slot_id)?;
            slot.mps = real_mps;
        }

        self.next_address += 1;

        SerialPort::puts("[xhci]  dev slot=");
        SerialPort::put_u64(slot_id as u64);
        SerialPort::puts(" vid=0x");
        SerialPort::put_hex(slot.vendor_id as u64);
        SerialPort::puts(" pid=0x");
        SerialPort::put_hex(slot.product_id as u64);
        SerialPort::puts("\n");

        self.slots.push(slot);
        Ok(())
    }
}
