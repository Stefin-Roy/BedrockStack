use alloc::vec::Vec;
use crate::drivers::serial::SerialPort;
use crate::services::dma::DmaAllocator;
use crate::usb::usb;
use crate::usb::usb::SetupPacket;
use crate::usb::usb::descriptors::{ConfigDescriptor, InterfaceDescriptor, EndpointDescriptor};
use crate::usb::xhci::command;
use crate::usb::xhci::context::{self, EndpointConfig};
use crate::usb::xhci::event;
use crate::usb::xhci::memory::{self, TrbRing};

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
    pub interface_class: u8,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    pub bulk_in_dci: u8,
    pub bulk_out_dci: u8,
    pub bulk_in_mps: u16,
    pub bulk_out_mps: u16,
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
            interface_class: 0,
            interface_subclass: 0,
            interface_protocol: 0,
            bulk_in_dci: 0,
            bulk_out_dci: 0,
            bulk_in_mps: 0,
            bulk_out_mps: 0,
        }
    }
}

fn wait_for_transfer(slot_id: u8, ep_id: u8) -> Result<u8, &'static str> {
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + 5_000_000_000;
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
    if cc == 1 || cc == 13 {
        Ok(cc)
    } else {
        Err("transfer failed")
    }
}

fn submit_control(
    slot: &mut DeviceSlot,
    doorbell_va: u64,
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

    slot.ep0_ring.enqueue(&memory::make_setup_stage_trb(&setup_raw, trt));

    if data_len > 0 {
        slot.ep0_ring.enqueue(&memory::make_data_stage_trb(data_phys, data_len as u32, dir_in));
    }

    slot.ep0_ring.enqueue(&memory::make_status_stage_trb(dir_in));

    slot.ep0_ring.flush();
    command::ring_doorbell(doorbell_va, slot.slot_id, 1);

    let cc = wait_for_transfer(slot.slot_id, 1)?;
    Ok(cc as u32)
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
    submit_control(slot, doorbell_va, &setup_full, data_phys, total_len as u16, true)?;

    let cfg_buf = unsafe { core::slice::from_raw_parts(data_va as *const u8, total_len) };
    let limit = cfg_buf.len();

    if cfg!(feature = "usb_trace") {
        SerialPort::puts("[xhci]  config desc slot=");
        SerialPort::put_u64(slot.slot_id as u64);
        SerialPort::puts(" (");
        SerialPort::put_u64(total_len as u64);
        SerialPort::puts(" bytes)\n");
    }

    #[derive(Clone, Copy)]
    struct IfaceInfo {
        num: u8,
        class: u8,
        subclass: u8,
        protocol: u8,
        bulk_in_dci: u8,
        bulk_out_dci: u8,
        bulk_in_mps: u16,
        bulk_out_mps: u16,
    }
    let zero = IfaceInfo {
        num: 0, class: 0, subclass: 0, protocol: 0,
        bulk_in_dci: 0, bulk_out_dci: 0,
        bulk_in_mps: 0, bulk_out_mps: 0,
    };
    let mut ifaces = [zero; 8];
    let mut iface_count: u8 = 0;
    let mut cur_iface_num: u8 = 0;
    let mut cur_alt_setting: u8 = 0;

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
                    if cur_alt_setting == 0 && (iface_count as usize) < ifaces.len() {
                        let idx = iface_count as usize;
                        ifaces[idx].num = cur_iface_num;
                        ifaces[idx].class = iface.class();
                        ifaces[idx].subclass = iface.subclass();
                        ifaces[idx].protocol = iface.protocol();
                        iface_count += 1;
                        if cfg!(feature = "usb_trace") {
                            SerialPort::puts("[xhci]    iface ");
                            SerialPort::put_u64(iface.interface_number() as u64);
                            SerialPort::puts(": class=0x");
                            SerialPort::put_hex(iface.class() as u64);
                            SerialPort::puts(" subclass=0x");
                            SerialPort::put_hex(iface.subclass() as u64);
                            SerialPort::puts(" protocol=0x");
                            SerialPort::put_hex(iface.protocol() as u64);
                            SerialPort::puts("\n");
                        }
                    }
                }
            }
            usb::DESC_ENDPOINT => {
                if cur_alt_setting == 0 {
                    if let Some(ep) = EndpointDescriptor::parse(&cfg_buf[offset..]) {
                        if ep.transfer_type() == usb::EP_TYPE_BULK {
                            let ep_num = ep.endpoint_number();
                            let is_in = ep.is_in();
                            let dci: u8 = (ep_num * 2) + if is_in { 1 } else { 0 };
                            for i in 0..(iface_count as usize) {
                                if ifaces[i].num == cur_iface_num {
                                    if is_in {
                                        ifaces[i].bulk_in_dci = dci;
                                        ifaces[i].bulk_in_mps = ep.max_packet_size();
                                    } else {
                                        ifaces[i].bulk_out_dci = dci;
                                        ifaces[i].bulk_out_mps = ep.max_packet_size();
                                    }
                                    if cfg!(feature = "usb_trace") {
                                        SerialPort::puts("[xhci]      bulk ");
                                        SerialPort::puts(if is_in { "IN " } else { "OUT" });
                                        SerialPort::puts(" dci=");
                                        SerialPort::put_u64(dci as u64);
                                        SerialPort::puts(" mps=");
                                        SerialPort::put_u64(ep.max_packet_size() as u64);
                                        SerialPort::puts("\n");
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        offset += len;
    }

    // Prefer mass storage interface; fall back to first non-zero class,
    // or interface 0 if all interfaces report class=0 (device-level class).
    let mut chosen: Option<usize> = None;
    for i in 0..(iface_count as usize) {
        if ifaces[i].class == usb::CLASS_MASS_STORAGE {
            chosen = Some(i);
            break;
        }
    }
    let chosen = chosen.or_else(|| {
        let idx = (0..(iface_count as usize)).position(|i| ifaces[i].class != 0);
        idx.or(if iface_count > 0 { Some(0) } else { None })
    });

    if let Some(idx) = chosen {
        slot.interface_class = ifaces[idx].class;
        slot.interface_subclass = ifaces[idx].subclass;
        slot.interface_protocol = ifaces[idx].protocol;
        slot.bulk_in_dci = ifaces[idx].bulk_in_dci;
        slot.bulk_out_dci = ifaces[idx].bulk_out_dci;
        slot.bulk_in_mps = ifaces[idx].bulk_in_mps;
        slot.bulk_out_mps = ifaces[idx].bulk_out_mps;
        if cfg!(feature = "usb_trace") {
            SerialPort::puts("[xhci]  selected iface ");
            SerialPort::put_u64(ifaces[idx].num as u64);
            if ifaces[idx].class == usb::CLASS_MASS_STORAGE {
                SerialPort::puts(" (mass storage)\n");
            } else {
                SerialPort::puts(" (fallback)\n");
            }
        }
    } else if cfg!(feature = "usb_trace") {
        SerialPort::puts("[xhci]  no usable interface found\n");
    }

    Ok(())
}

pub fn configure_device(
    slot: &mut DeviceSlot,
    cmd_ring: &mut TrbRing,
    doorbell_va: u64,
    dma: &dyn DmaAllocator,
) -> Result<(), &'static str> {
    // Per xHCI §4.8.1, SET_CONFIGURATION to the USB device MUST precede
    // the Configure Endpoint command to the xHC.
    if slot.config_value != 0 {
        let setup = SetupPacket::set_configuration(slot.config_value);
        submit_control(slot, doorbell_va, &setup, 0, 0, false)?;
    }

    let icc_va = slot.icc_va;
    let ctx_size = slot.ctx_size;
    unsafe { core::ptr::write_bytes(icc_va as *mut u8, 0, 4096) };

    let mut ring_pairs: Vec<(u8, TrbRing)> = Vec::new();
    let mut endpoints: Vec<EndpointConfig> = Vec::new();

    if slot.bulk_out_dci != 0 {
        let ring = TrbRing::new(dma, 4096)?;
        let dci = slot.bulk_out_dci;
        endpoints.push(EndpointConfig {
            dci,
            ep_type: context::EP_TYPE_BULK_OUT,
            max_packet_size: slot.bulk_out_mps,
            dequeue_phys: ring.phys,
            cerr: 3,
            avg_trb_len: 3072,
            max_burst: 0,
            interval: 0,
        });
        ring_pairs.push((dci, ring));
    }

    if slot.bulk_in_dci != 0 {
        let ring = TrbRing::new(dma, 4096)?;
        let dci = slot.bulk_in_dci;
        endpoints.push(EndpointConfig {
            dci,
            ep_type: context::EP_TYPE_BULK_IN,
            max_packet_size: slot.bulk_in_mps,
            dequeue_phys: ring.phys,
            cerr: 3,
            avg_trb_len: 3072,
            max_burst: 0,
            interval: 0,
        });
        ring_pairs.push((dci, ring));
    }

    // Only push to slot.ep_rings once all allocations succeeded.
    slot.ep_rings.extend(ring_pairs);

    context::init_icc_for_configure_endpoint(icc_va, ctx_size, slot.speed, slot.port_num, &endpoints);

    command::submit_configure_endpoint(cmd_ring, doorbell_va, slot.icc_phys, slot.slot_id, false)?;

    SerialPort::puts("[xhci]  configured slot=");
    SerialPort::put_u64(slot.slot_id as u64);
    SerialPort::puts(" class=");
    SerialPort::put_u64(slot.interface_class as u64);
    if slot.bulk_out_dci != 0 {
        SerialPort::puts(" bulk_out_dci=");
        SerialPort::put_u64(slot.bulk_out_dci as u64);
    }
    if slot.bulk_in_dci != 0 {
        SerialPort::puts(" bulk_in_dci=");
        SerialPort::put_u64(slot.bulk_in_dci as u64);
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

pub fn find_ep_ring(slot: &DeviceSlot, dci: u8) -> Option<&TrbRing> {
    if dci == 1 {
        return Some(&slot.ep0_ring);
    }
    slot.ep_rings.iter().find(|(d, _)| *d == dci).map(|(_, r)| r)
}

pub fn find_ep_ring_mut(slot: &mut DeviceSlot, dci: u8) -> Option<&mut TrbRing> {
    if dci == 1 {
        return Some(&mut slot.ep0_ring);
    }
    slot.ep_rings.iter_mut().find(|(d, _)| *d == dci).map(|(_, r)| r)
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
        context::init_icc_for_address_device(icc_buf.virt, self.ctx_size, speed, port_num, mps, ep0_ring.phys);
        command::submit_address_device(cmd_ring, doorbell_va, icc_buf.phys, slot_id, bsr)?;

        let mut slot = DeviceSlot::new(
            slot_id, port_num, speed, self.ctx_size, mps,
            icc_buf.phys, icc_buf.virt, ep0_ring, address,
        );

        if bsr {
            // Read first 8 bytes of device descriptor to discover bMaxPacketSize0
            // (USB 2.0 §9.4.3).  Reading 18 bytes with unknown MPS is unsafe — the
            // xHC may mishandle the split transfer on some implementations.
            let setup8 = SetupPacket::get_descriptor(usb::DESC_DEVICE, 0, 0, 8);
            submit_control(&mut slot, doorbell_va, &setup8, desc_buf.phys, 8, true)?;
            let desc_mps_raw = unsafe { core::ptr::read_volatile((desc_buf.virt + 7) as *const u8) };
            let real_mps = if desc_mps_raw < 8 { 8 } else { desc_mps_raw as u16 };

            // Re-address with correct MPS and real device address (BSR=0).
            unsafe { core::ptr::write_bytes(icc_buf.virt as *mut u8, 0, 4096) };
            context::init_icc_for_address_device(icc_buf.virt, self.ctx_size, speed, port_num, real_mps, slot.ep0_ring.phys);
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
            context::init_icc_for_evaluate_ep0(icc_buf.virt, self.ctx_size, speed, port_num, real_mps, slot.ep0_ring.phys);
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
