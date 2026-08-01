use core::sync::atomic::{AtomicU32, Ordering};

use framebuffer::Framebuffer;
use crate::drivers::serial::SerialPort;
use crate::usb::usb;
use crate::usb::usb::descriptors;
use crate::usb::xhci::memory::{self, Trb};
use crate::usb::xhci::context;
use super::Module;

static PASS: AtomicU32 = AtomicU32::new(0);
static SKIP: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);

macro_rules! t {
    ($name:expr, $body:expr) => {
        {
            let mut port = SerialPort::new();
            use core::fmt::Write;
            write!(port, "[USBTEST] {:35} ", $name).ok();
            match $body {
                Ok(()) => {
                    write!(port, "PASS\n").ok();
                    PASS.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    write!(port, "FAIL: {}\n", e).ok();
                    FAIL.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    };
}

fn test_usb_constants() -> Result<(), &'static str> {
    if usb::USB_DIR_OUT != 0 { return Err("USB_DIR_OUT wrong"); }
    if usb::USB_DIR_IN != 1 { return Err("USB_DIR_IN wrong"); }
    if usb::USB_TYPE_STANDARD != 0 { return Err("USB_TYPE_STANDARD wrong"); }
    if usb::DESC_DEVICE != 1 { return Err("DESC_DEVICE wrong"); }
    if usb::DESC_CONFIG != 2 { return Err("DESC_CONFIG wrong"); }
    if usb::DESC_INTERFACE != 4 { return Err("DESC_INTERFACE wrong"); }
    if usb::DESC_ENDPOINT != 5 { return Err("DESC_ENDPOINT wrong"); }
    if usb::DESC_SS_EP_COMPANION != 48 { return Err("DESC_SS_EP_COMPANION wrong"); }
    if usb::REQ_GET_DESCRIPTOR != 6 { return Err("REQ_GET_DESCRIPTOR wrong"); }
    if usb::REQ_SET_ADDRESS != 5 { return Err("REQ_SET_ADDRESS wrong"); }
    if usb::REQ_SET_CONFIGURATION != 9 { return Err("REQ_SET_CONFIGURATION wrong"); }
    if usb::SPEED_LS != 1 { return Err("SPEED_LS wrong"); }
    if usb::SPEED_FS != 2 { return Err("SPEED_FS wrong"); }
    if usb::SPEED_HS != 3 { return Err("SPEED_HS wrong"); }
    if usb::SPEED_SS != 4 { return Err("SPEED_SS wrong"); }
    if usb::CLASS_HUB != 9 { return Err("CLASS_HUB wrong"); }
    if usb::CLASS_MASS_STORAGE != 8 { return Err("CLASS_MASS_STORAGE wrong"); }
    if usb::CLASS_HID != 3 { return Err("CLASS_HID wrong"); }
    if usb::EP_TYPE_CONTROL != 0 { return Err("EP_TYPE_CONTROL wrong"); }
    if usb::EP_TYPE_BULK != 2 { return Err("EP_TYPE_BULK wrong"); }
    if usb::EP_TYPE_INTERRUPT != 3 { return Err("EP_TYPE_INTERRUPT wrong"); }
    Ok(())
}

fn test_setup_packet_get_descriptor() -> Result<(), &'static str> {
    let pkt = usb::SetupPacket::get_descriptor(usb::DESC_DEVICE, 0, 0, 18);
    let expected_bm = usb::BMREQ_DEVICE_TO_HOST | usb::USB_TYPE_STANDARD | usb::USB_RECIP_DEVICE;
    if pkt.bm_request_type != expected_bm { return Err("bm_request_type mismatch"); }
    if pkt.b_request != usb::REQ_GET_DESCRIPTOR { return Err("b_request mismatch"); }
    if pkt.w_value != (usb::DESC_DEVICE as u16) << 8 { return Err("w_value mismatch"); }
    if pkt.w_index != 0 { return Err("w_index mismatch"); }
    if pkt.w_length != 18 { return Err("w_length mismatch"); }
    Ok(())
}

fn test_setup_packet_set_address() -> Result<(), &'static str> {
    let pkt = usb::SetupPacket::set_address(42);
    let expected_bm = usb::BMREQ_HOST_TO_DEVICE | usb::USB_TYPE_STANDARD | usb::USB_RECIP_DEVICE;
    if pkt.bm_request_type != expected_bm { return Err("bm_request_type mismatch"); }
    if pkt.b_request != usb::REQ_SET_ADDRESS { return Err("b_request mismatch"); }
    if pkt.w_value != 42 { return Err("w_value mismatch"); }
    if pkt.w_length != 0 { return Err("w_length should be 0"); }
    Ok(())
}

fn test_setup_packet_set_configuration() -> Result<(), &'static str> {
    let pkt = usb::SetupPacket::set_configuration(1);
    if pkt.b_request != usb::REQ_SET_CONFIGURATION { return Err("b_request mismatch"); }
    if pkt.w_value != 1 { return Err("w_value mismatch"); }
    if pkt.w_length != 0 { return Err("w_length should be 0"); }
    Ok(())
}

fn test_setup_packet_set_interface() -> Result<(), &'static str> {
    let pkt = usb::SetupPacket::set_interface(0, 1);
    let expected_bm = usb::BMREQ_HOST_TO_DEVICE | usb::USB_TYPE_STANDARD | usb::USB_RECIP_INTERFACE;
    if pkt.bm_request_type != expected_bm { return Err("bm_request_type mismatch"); }
    if pkt.b_request != usb::REQ_SET_INTERFACE { return Err("b_request mismatch"); }
    if pkt.w_value != 1 { return Err("alt_setting not in w_value"); }
    if pkt.w_index != 0 { return Err("interface not in w_index"); }
    Ok(())
}

fn test_setup_packet_clear_feature() -> Result<(), &'static str> {
    let pkt = usb::SetupPacket::clear_feature(usb::USB_RECIP_ENDPOINT, usb::FEATURE_ENDPOINT_HALT, 0x81);
    let expected_bm = usb::BMREQ_HOST_TO_DEVICE | usb::USB_TYPE_STANDARD | usb::USB_RECIP_ENDPOINT;
    if pkt.bm_request_type != expected_bm { return Err("bm_request_type mismatch"); }
    if pkt.b_request != usb::REQ_CLEAR_FEATURE { return Err("b_request mismatch"); }
    if pkt.w_value != usb::FEATURE_ENDPOINT_HALT as u16 { return Err("w_value mismatch"); }
    if pkt.w_index != 0x81 { return Err("w_index mismatch"); }
    Ok(())
}

fn test_setup_packet_get_configuration() -> Result<(), &'static str> {
    let pkt = usb::SetupPacket::get_configuration(1);
    let expected_bm = usb::BMREQ_DEVICE_TO_HOST | usb::USB_TYPE_STANDARD | usb::USB_RECIP_DEVICE;
    if pkt.bm_request_type != expected_bm { return Err("bm_request_type mismatch"); }
    if pkt.b_request != usb::REQ_GET_CONFIGURATION { return Err("b_request mismatch"); }
    if pkt.w_length != 1 { return Err("w_length mismatch"); }
    Ok(())
}

fn test_device_descriptor_parse() -> Result<(), &'static str> {
    let raw: [u8; 18] = [
        18, 1,        // bLength=18, bDescriptorType=DEVICE
        0x00, 0x02,   // bcdUSB=0x0200
        0x00,         // bDeviceClass
        0x00,         // bDeviceSubClass
        0x00,         // bDeviceProtocol
        64,           // bMaxPacketSize0=64
        0x09, 0x12,   // idVendor=0x1209
        0x00, 0x00,   // idProduct
        0x00, 0x01,   // bcdDevice=0x0100
        0, 0, 0,      // iManufacturer, iProduct, iSerialNumber
        1,            // bNumConfigurations
    ];
    let desc = descriptors::DeviceDescriptor::parse(&raw).ok_or("parse returned None")?;
    if desc.b_length != 18 { return Err("bLength wrong"); }
    if desc.b_descriptor_type != usb::DESC_DEVICE { return Err("desc type wrong"); }
    if desc.b_max_packet_size0 != 64 { return Err("max packet size wrong"); }
    if desc.id_vendor != 0x1209 { return Err("idVendor wrong"); }
    if desc.b_num_configurations != 1 { return Err("num configs wrong"); }
    Ok(())
}

fn test_device_descriptor_parse_short() -> Result<(), &'static str> {
    let raw = [17, 1, 0, 2, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    if descriptors::DeviceDescriptor::parse(&raw).is_some() {
        return Err("should have failed on short data");
    }
    Ok(())
}

fn test_device_descriptor_parse_wrong_type() -> Result<(), &'static str> {
    let raw = [18, 2, 0, 2, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    if descriptors::DeviceDescriptor::parse(&raw).is_some() {
        return Err("should have failed on wrong descriptor type");
    }
    Ok(())
}

fn test_config_descriptor_parse() -> Result<(), &'static str> {
    let raw: [u8; 9] = [
        9, 2,         // bLength=9, bDescriptorType=CONFIG
        0x12, 0x00,   // wTotalLength=18
        1,            // bNumInterfaces=1
        1,            // bConfigurationValue=1
        0,            // iConfiguration
        0x80,         // bmAttributes=self-powered
        50,           // bMaxPower=100mA
    ];
    let desc = descriptors::ConfigDescriptor::parse(&raw).ok_or("parse returned None")?;
    if desc.total_length() != 18 { return Err("total_length wrong"); }
    if desc.num_interfaces() != 1 { return Err("num_interfaces wrong"); }
    if desc.configuration_value() != 1 { return Err("configuration_value wrong"); }
    if desc.max_power() != 50 { return Err("max_power wrong"); }
    if desc.attributes() != 0x80 { return Err("attributes wrong"); }
    Ok(())
}

fn test_interface_descriptor_parse() -> Result<(), &'static str> {
    let raw: [u8; 9] = [
        9, 4,         // bLength=9, bDescriptorType=INTERFACE
        0,            // bInterfaceNumber=0
        0,            // bAlternateSetting=0
        2,            // bNumEndpoints=2
        0x08,         // bInterfaceClass=MASS_STORAGE
        0x06,         // bInterfaceSubClass=SCSI
        0x50,         // bInterfaceProtocol=Bulk-Only
        0,            // iInterface
    ];
    let desc = descriptors::InterfaceDescriptor::parse(&raw).ok_or("parse returned None")?;
    if desc.interface_number() != 0 { return Err("interface_number wrong"); }
    if desc.num_endpoints() != 2 { return Err("num_endpoints wrong"); }
    if desc.class() != usb::CLASS_MASS_STORAGE { return Err("class wrong"); }
    if desc.subclass() != 0x06 { return Err("subclass wrong"); }
    if desc.protocol() != 0x50 { return Err("protocol wrong"); }
    Ok(())
}

fn test_endpoint_descriptor_parse() -> Result<(), &'static str> {
    let raw: [u8; 7] = [
        7, 5,         // bLength=7, bDescriptorType=ENDPOINT
        0x81,         // bEndpointAddress=EP1 IN
        0x02,         // bmAttributes=BULK
        0x00, 0x02,   // wMaxPacketSize=512
        0,            // bInterval
    ];
    let desc = descriptors::EndpointDescriptor::parse(&raw).ok_or("parse returned None")?;
    if desc.endpoint_number() != 1 { return Err("endpoint_number wrong"); }
    if !desc.is_in() { return Err("should be IN"); }
    if desc.transfer_type() != usb::EP_TYPE_BULK { return Err("transfer_type wrong"); }
    if desc.max_packet_size() != 512 { return Err("max_packet_size wrong"); }
    Ok(())
}

fn test_ss_ep_companion_parse() -> Result<(), &'static str> {
    let raw: [u8; 6] = [
        6, 48,        // bLength=6, bDescriptorType=SS_EP_COMPANION
        16,           // bMaxBurst=16
        0,            // bmAttributes
        0, 0,         // wBytesPerInterval
    ];
    let desc = descriptors::SsEndpointCompanionDescriptor::parse(&raw).ok_or("parse returned None")?;
    if desc.max_burst() != 16 { return Err("max_burst wrong"); }
    Ok(())
}

fn test_trb_factory_enable_slot() -> Result<(), &'static str> {
    let trb = memory::make_enable_slot_trb();
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_ENABLE_SLOT { return Err("wrong TRB type"); }
    if trb.parameter != 0 { return Err("param not zero"); }
    if trb.status != 0 { return Err("status not zero"); }
    if trb.control & memory::TRB_IOC != 0 { return Err("enable slot should not have IOC"); }
    Ok(())
}

fn test_trb_factory_address_device() -> Result<(), &'static str> {
    let ctx_phys: u64 = 0x10000;
    let trb = memory::make_address_device_trb(ctx_phys, 1, false);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_ADDRESS_DEVICE { return Err("wrong TRB type"); }
    if trb.parameter != ctx_phys { return Err("param mismatch"); }
    if (trb.control >> 24) & 0xFF != 1 { return Err("slot_id wrong"); }
    if trb.control & memory::TRB_BSR != 0 { return Err("BSR should not be set"); }
    Ok(())
}

fn test_trb_factory_address_device_bsr() -> Result<(), &'static str> {
    let trb = memory::make_address_device_trb(0x20000, 2, true);
    if trb.control & memory::TRB_BSR == 0 { return Err("BSR should be set"); }
    if (trb.control >> 24) & 0xFF != 2 { return Err("slot_id wrong"); }
    Ok(())
}

fn test_trb_factory_configure_endpoint() -> Result<(), &'static str> {
    let trb = memory::make_configure_endpoint_trb(0x30000, 3, false);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_CONFIGURE_ENDPOINT { return Err("wrong TRB type"); }
    if trb.parameter != 0x30000 { return Err("param mismatch"); }
    if (trb.control >> 24) & 0xFF != 3 { return Err("slot_id wrong"); }
    if trb.control & memory::TRB_DC != 0 { return Err("DC should not be set"); }
    Ok(())
}

fn test_trb_factory_configure_endpoint_deconfig() -> Result<(), &'static str> {
    let trb = memory::make_configure_endpoint_trb(0, 1, true);
    if trb.control & memory::TRB_DC == 0 { return Err("DC should be set"); }
    Ok(())
}

fn test_trb_factory_normal() -> Result<(), &'static str> {
    let trb = memory::make_normal_trb(0x40000, 1024);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_NORMAL { return Err("wrong TRB type"); }
    if trb.parameter != 0x40000 { return Err("data_phys mismatch"); }
    if trb.status != 1024 { return Err("length mismatch"); }
    let slot_id = (trb.control >> 24) & 0xFF;
    let ep_id = (trb.control >> 16) & 0xFF;
    if slot_id != 0 { return Err("slot_id wrong"); }
    if ep_id != 0 { return Err("ep_id wrong"); }
    if trb.control & memory::TRB_IOC == 0 { return Err("missing IOC"); }
    Ok(())
}

fn test_trb_factory_setup_stage() -> Result<(), &'static str> {
    let setup = [0x80u8, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
    let trb = memory::make_setup_stage_trb(&setup, 3);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_SETUP_STAGE { return Err("wrong TRB type"); }
    let trt = (trb.control >> 16) & 0x3;
    if trt != 3 { return Err("TRT wrong"); }
    if trb.status != 8 { return Err("status should be 8 (setup size)"); }
    if trb.control & memory::TRB_IDT == 0 { return Err("missing IDT"); }
    Ok(())
}

fn test_trb_factory_data_stage() -> Result<(), &'static str> {
    let trb = memory::make_data_stage_trb(0x50000, 256, true);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_DATA_STAGE { return Err("wrong TRB type"); }
    if trb.parameter != 0x50000 { return Err("data_phys mismatch"); }
    if trb.status != 256 { return Err("length mismatch"); }
    if trb.control & memory::TRB_DIR_IN == 0 { return Err("missing DIR_IN"); }
    Ok(())
}

fn test_trb_factory_data_stage_out() -> Result<(), &'static str> {
    let trb = memory::make_data_stage_trb(0x60000, 64, false);
    if trb.control & memory::TRB_DIR_IN != 0 { return Err("DIR_IN should not be set"); }
    Ok(())
}

fn test_trb_factory_status_stage() -> Result<(), &'static str> {
    let trb = memory::make_status_stage_trb(true);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_STATUS_STAGE { return Err("wrong TRB type"); }
    if trb.control & memory::TRB_DIR_IN != 0 { return Err("DIR_IN should not be set (status is OUT for dir_in data)"); }
    if trb.control & memory::TRB_IOC == 0 { return Err("missing IOC"); }
    Ok(())
}

fn test_trb_factory_status_stage_out() -> Result<(), &'static str> {
    let trb = memory::make_status_stage_trb(false);
    if trb.control & memory::TRB_DIR_IN == 0 { return Err("DIR_IN should be set (status is IN for dir_out data)"); }
    Ok(())
}

fn test_trb_factory_evaluate_context() -> Result<(), &'static str> {
    let trb = memory::make_evaluate_context_trb(0x70000, 4);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_EVALUATE_CONTEXT { return Err("wrong TRB type"); }
    if trb.parameter != 0x70000 { return Err("param mismatch"); }
    Ok(())
}

fn test_trb_factory_no_op() -> Result<(), &'static str> {
    let trb = memory::make_no_op_command_trb();
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_NO_OP_COMMAND { return Err("wrong TRB type"); }
    if trb.control & memory::TRB_IOC == 0 { return Err("missing IOC"); }
    Ok(())
}

fn test_trb_factory_disable_slot() -> Result<(), &'static str> {
    let trb = memory::make_disable_slot_trb(5);
    let ttype = trb.trb_type();
    if ttype != memory::TRB_TYPE_DISABLE_SLOT { return Err("wrong TRB type"); }
    let slot_id = (trb.control >> 24) & 0xFF;
    if slot_id != 5 { return Err("slot_id not encoded"); }
    Ok(())
}

fn test_trb_struct_methods() -> Result<(), &'static str> {
    let trb = Trb::new(0x1234, 0x5678, (9 << 10) | 1);
    if trb.parameter != 0x1234 { return Err("parameter mismatch"); }
    if trb.status != 0x5678 { return Err("status mismatch"); }
    let ttype = trb.trb_type();
    if ttype != 9 { return Err("trb_type should be 9 (ENABLE_SLOT)"); }
    if !trb.cycle_bit() { return Err("cycle_bit should be true"); }
    if trb.is_event() { return Err("type 9 is not an event"); }
    Ok(())
}

fn test_trb_is_event() -> Result<(), &'static str> {
    let ev = Trb::new(0, 0, (33 << 10) | 1);
    if !ev.is_event() { return Err("type 33 should be event"); }
    let cmd = Trb::new(0, 0, (9 << 10) | 1);
    if cmd.is_event() { return Err("type 9 should not be event"); }
    Ok(())
}

fn test_trb_cycle_bit() -> Result<(), &'static str> {
    let with = Trb::new(0, 0, 1);
    if !with.cycle_bit() { return Err("should have cycle=1"); }
    let without = Trb::new(0, 0, 0);
    if without.cycle_bit() { return Err("should have cycle=0"); }
    Ok(())
}

fn test_trb_type_constants() -> Result<(), &'static str> {
    if memory::TRB_TYPE_NORMAL != 1 { return Err("TRB_TYPE_NORMAL wrong"); }
    if memory::TRB_TYPE_SETUP_STAGE != 2 { return Err("TRB_TYPE_SETUP_STAGE wrong"); }
    if memory::TRB_TYPE_DATA_STAGE != 3 { return Err("TRB_TYPE_DATA_STAGE wrong"); }
    if memory::TRB_TYPE_STATUS_STAGE != 4 { return Err("TRB_TYPE_STATUS_STAGE wrong"); }
    if memory::TRB_TYPE_NO_OP != 8 { return Err("TRB_TYPE_NO_OP wrong"); }
    if memory::TRB_TYPE_ENABLE_SLOT != 9 { return Err("TRB_TYPE_ENABLE_SLOT wrong"); }
    if memory::TRB_TYPE_ADDRESS_DEVICE != 11 { return Err("TRB_TYPE_ADDRESS_DEVICE wrong"); }
    if memory::TRB_TYPE_CONFIGURE_ENDPOINT != 12 { return Err("TRB_TYPE_CONFIGURE_ENDPOINT wrong"); }
    if memory::TRB_TYPE_EVALUATE_CONTEXT != 13 { return Err("TRB_TYPE_EVALUATE_CONTEXT wrong"); }
    if memory::TRB_TYPE_NO_OP_COMMAND != 23 { return Err("TRB_TYPE_NO_OP_COMMAND wrong"); }
    Ok(())
}

fn test_trb_flag_constants() -> Result<(), &'static str> {
    if memory::TRB_CYCLE != 1 << 0 { return Err("TRB_CYCLE wrong"); }
    if memory::TRB_CHAIN != 1 << 4 { return Err("TRB_CHAIN wrong"); }
    if memory::TRB_ENT != 1 << 1 { return Err("TRB_ENT wrong"); }
    if memory::TRB_IDT != 1 << 6 { return Err("TRB_IDT wrong"); }
    if memory::TRB_SIA != 1u32 << 31 { return Err("TRB_SIA wrong"); }
    if memory::TRB_BSR != 1 << 9 { return Err("TRB_BSR wrong"); }
    if memory::TRB_DC != 1 << 9 { return Err("TRB_DC wrong"); }
    if memory::TRB_DIR_IN != 1 << 16 { return Err("TRB_DIR_IN wrong"); }
    Ok(())
}

fn test_event_completion_constants() -> Result<(), &'static str> {
    if memory::CMD_INVALID != 0 { return Err("CMD_INVALID wrong"); }
    if memory::CMD_SUCCESS != 1 { return Err("CMD_SUCCESS wrong"); }
    if memory::CMD_TRB_ERROR != 5 { return Err("CMD_TRB_ERROR wrong"); }
    if memory::CMD_STALL_ERROR != 6 { return Err("CMD_STALL_ERROR wrong"); }
    if memory::CMD_RESOURCE_ERROR != 7 { return Err("CMD_RESOURCE_ERROR wrong"); }
    if memory::CMD_CONTEXT_STATE_ERROR != 18 { return Err("CMD_CONTEXT_STATE_ERROR wrong"); }
    if memory::EVT_TRANSFER != (32 << 10) as u32 { return Err("EVT_TRANSFER wrong"); }
    if memory::EVT_COMMAND_COMPLETION != (33 << 10) as u32 { return Err("EVT_COMMAND_COMPLETION wrong"); }
    if memory::EVT_PORT_STATUS_CHANGE != (34 << 10) as u32 { return Err("EVT_PORT_STATUS_CHANGE wrong"); }
    Ok(())
}

fn read32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn test_init_icc_for_address_device() -> Result<(), &'static str> {
    // The ICC must be built identically for 32-byte (CSZ=0) and 64-byte
    // (CSZ=1) contexts, with the slot context at 32 and EP0 at 32+ctx_size.
    for ctx_size in [32u8, 64u8] {
        let mut buf = [0u8; 4096];
        let icc_va = buf.as_mut_ptr() as usize as u64;
        context::init_icc_for_address_device(icc_va, ctx_size, 3, 1, 64, 0x10000);

        if read32(&buf, 0x00) != 0 {
            return Err("drop_flags should be 0");
        }
        if read32(&buf, 0x04) != 0x3 {
            return Err("add_flags should be 0x3");
        }

        let entries = (read32(&buf, 0x20) >> 27) & 0x1F;
        let speed = (read32(&buf, 0x20) >> 20) & 0xF;
        let port_num = (read32(&buf, 0x20 + 0x04) >> 16) & 0xFF;
        if speed != 3 {
            return Err("slot speed wrong");
        }
        if port_num != 1 {
            return Err("slot port_num wrong");
        }
        if entries != 1 {
            return Err("slot entries wrong");
        }

        let ep0_base = 0x20 + ctx_size as usize;
        let ep_dw1 = read32(&buf, ep0_base + 0x04);
        let mps = (ep_dw1 >> 16) & 0xFFFF;
        let ep_type = (ep_dw1 >> 3) & 0x7;
        let cerr = (ep_dw1 >> 1) & 0x3;
        if mps != 64 {
            return Err("EP0 MPS wrong");
        }
        if ep_type != 4 {
            return Err("EP0 type not control (expected 4 per xHCI spec)");
        }
        if cerr != 3 {
            return Err("EP0 CErr wrong");
        }

        let dequeue = (read32(&buf, ep0_base + 0x08) as u64)
            | ((read32(&buf, ep0_base + 0x0C) as u64) << 32);
        if dequeue & 1 != 1 {
            return Err("EP0 DCS not set");
        }
        if dequeue & !1 != 0x10000 {
            return Err("EP0 dequeue ptr wrong");
        }

        let dw4 = read32(&buf, ep0_base + 0x10);
        if dw4 & 0xFFFF != 8 {
            return Err("EP0 avg_trb_len wrong");
        }
    }
    Ok(())
}

pub struct UsbTest;

impl Module for UsbTest {
    fn name(&self) -> &str { "usb_test" }

    fn version(&self) -> &str { "0.1.0" }

    fn init(&self, _display: &mut Framebuffer) -> Result<(), &'static str> {
        SerialPort::puts("[USBTEST] === USB Subsystem Test Suite ===\n");

        t!("usb_constants", test_usb_constants());
        t!("setup_get_descriptor", test_setup_packet_get_descriptor());
        t!("setup_set_address", test_setup_packet_set_address());
        t!("setup_set_config", test_setup_packet_set_configuration());
        t!("setup_set_interface", test_setup_packet_set_interface());
        t!("setup_clear_feature", test_setup_packet_clear_feature());
        t!("setup_get_config", test_setup_packet_get_configuration());
        t!("dev_desc_parse", test_device_descriptor_parse());
        t!("dev_desc_short", test_device_descriptor_parse_short());
        t!("dev_desc_wrong_type", test_device_descriptor_parse_wrong_type());
        t!("cfg_desc_parse", test_config_descriptor_parse());
        t!("iface_desc_parse", test_interface_descriptor_parse());
        t!("ep_desc_parse", test_endpoint_descriptor_parse());
        t!("ss_ep_comp_parse", test_ss_ep_companion_parse());
        t!("trb_enable_slot", test_trb_factory_enable_slot());
        t!("trb_address_device", test_trb_factory_address_device());
        t!("trb_address_device_bsr", test_trb_factory_address_device_bsr());
        t!("trb_configure_ep", test_trb_factory_configure_endpoint());
        t!("trb_configure_ep_decfg", test_trb_factory_configure_endpoint_deconfig());
        t!("trb_normal", test_trb_factory_normal());
        t!("trb_setup_stage", test_trb_factory_setup_stage());
        t!("trb_data_stage", test_trb_factory_data_stage());
        t!("trb_data_stage_out", test_trb_factory_data_stage_out());
        t!("trb_status_stage", test_trb_factory_status_stage());
        t!("trb_status_stage_out", test_trb_factory_status_stage_out());
        t!("trb_eval_context", test_trb_factory_evaluate_context());
        t!("trb_no_op", test_trb_factory_no_op());
        t!("trb_disable_slot", test_trb_factory_disable_slot());
        t!("trb_struct_methods", test_trb_struct_methods());
        t!("trb_is_event", test_trb_is_event());
        t!("trb_cycle_bit", test_trb_cycle_bit());
        t!("trb_type_constants", test_trb_type_constants());
        t!("trb_flag_constants", test_trb_flag_constants());
        t!("event_completion_codes", test_event_completion_constants());
        t!("icc_address_device", test_init_icc_for_address_device());

        let p = PASS.load(Ordering::Relaxed);
        let s = SKIP.load(Ordering::Relaxed);
        let f = FAIL.load(Ordering::Relaxed);
        let mut port = SerialPort::new();
        use core::fmt::Write;
        write!(port, "[USBTEST] done: {}/{} passed", p, p + f).ok();
        if s > 0 { write!(port, " ({} skipped)", s).ok(); }
        if f > 0 { write!(port, " ({} FAILED)", f).ok(); }
        write!(port, "\n").ok();

        if f > 0 { Err("USB tests failed") } else { Ok(()) }
    }
}