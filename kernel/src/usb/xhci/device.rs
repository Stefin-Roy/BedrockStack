use alloc::vec::Vec;
use crate::drivers::serial::SerialPort;
use crate::usb::dma::UsbDmaAllocator;
use crate::usb::usb;
use crate::usb::usb::SetupPacket;
use crate::usb::xhci::command;
use crate::usb::xhci::context;
use crate::usb::xhci::event;
use crate::usb::xhci::memory::{self, TrbRing, InputControlContext};

pub struct UsbDevice {
    pub slot_id: u8,
    pub port_num: u8,
    pub speed: u8,
    pub max_packet_size0: u16,
}

impl UsbDevice {
    pub fn new(slot_id: u8, port_num: u8, speed: u8) -> Self {
        let max_pkt: u16 = match speed {
            usb::SPEED_LS => 8,
            usb::SPEED_FS => 64,
            usb::SPEED_HS => 64,
            usb::SPEED_SS => 512,
            _ => 64,
        };
        UsbDevice { slot_id, port_num, speed, max_packet_size0: max_pkt }
    }
}

pub struct DeviceSlot {
    pub slot_id: u8,
    pub port_num: u8,
    pub speed: u8,
    pub mps: u16,
    pub icc_phys: u64,
    pub icc_va: u64,
    pub ep0_ring: TrbRing,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
}

impl DeviceSlot {
    pub fn new(
        slot_id: u8,
        port_num: u8,
        speed: u8,
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
            mps,
            icc_phys,
            icc_va,
            ep0_ring,
            address,
            vendor_id: 0,
            product_id: 0,
        }
    }
}

fn wait_for_transfer(slot_id: u8, ep_id: u8) -> Result<u8, &'static str> {
    let mut timeout = crate::platform::x86_64_pc::apic::ApicTimeout::new(5000);
    loop {
        if let Some((sid, eid, cc, _remaining)) = event::last_transfer_completion() {
            if sid == slot_id && eid == ep_id {
                if cc == 1 || cc == 2 {
                    return Ok(cc);
                }
                return Err("transfer failed");
            }
        }
        event::consume_pending_events();
        if timeout.expired() {
            break;
        }
        core::hint::spin_loop();
    }
    Err("transfer timeout")
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
    let param = u64::from_le_bytes(setup_raw);
    let trt: u32 = if data_len == 0 {
        0
    } else if dir_in {
        2
    } else {
        1
    };

    let chain = if data_len > 0 { memory::TRB_CHAIN } else { 0 };
    let ioc_setup = if data_len == 0 { memory::TRB_IOC } else { 0 };
    let setup_control = (memory::TRB_TYPE_SETUP_STAGE as u32) << 10
        | trt
        | memory::TRB_IDT
        | chain
        | ioc_setup;
    slot.ep0_ring.enqueue_raw(param, 8, setup_control);

    if data_len > 0 {
        let data_control = (memory::TRB_TYPE_DATA_STAGE as u32) << 10
            | memory::TRB_CHAIN
            | if dir_in { memory::TRB_DIR_IN } else { 0 };
        slot.ep0_ring.enqueue_raw(data_phys, data_len as u32, data_control);

        let status_dir = if dir_in { 0 } else { memory::TRB_DIR_IN };
        let status_control = (memory::TRB_TYPE_STATUS_STAGE as u32) << 10
            | memory::TRB_IOC
            | status_dir;
        slot.ep0_ring.enqueue_raw(0, 0, status_control);
    }

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

pub struct DeviceSlotManager {
    pub slots: Vec<DeviceSlot>,
    next_address: u8,
}

impl DeviceSlotManager {
    pub fn new() -> Self {
        DeviceSlotManager {
            slots: Vec::new(),
            next_address: 1,
        }
    }

    pub fn enumerate_port(
        &mut self,
        cmd_ring: &mut TrbRing,
        doorbell_va: u64,
        dma: &mut UsbDmaAllocator,
        port_num: u8,
        speed: u8,
    ) -> Result<(), &'static str> {
        SerialPort::puts("[xhci] enumerate port ");
        SerialPort::put_u64(port_num as u64);
        SerialPort::puts(" speed=");
        SerialPort::put_u64(speed as u64);
        SerialPort::puts("\n");

        let slot_id = command::submit_enable_slot(cmd_ring, doorbell_va)?;

        let icc_buf = dma.alloc_page().ok_or("OOM for ICC")?;

        let ep0_ring = TrbRing::new(dma, 4096)?;

        let bsr = speed == usb::SPEED_FS;
        let mps_bsr: u16 = if bsr {
            8
        } else {
            match speed {
                usb::SPEED_LS => 8,
                usb::SPEED_HS => 64,
                usb::SPEED_SS => 512,
                _ => 64,
            }
        };

        let icc = unsafe { &mut *(icc_buf.virt as *mut InputControlContext) };
        *icc = InputControlContext::new_slot();
        context::init_icc_for_address_device(icc, speed, port_num, mps_bsr, ep0_ring.phys);

        let address = self.next_address;
        command::submit_address_device(cmd_ring, doorbell_va, icc_buf.phys, slot_id, bsr)?;

        let mps = if bsr { 8 } else { mps_bsr };
        let mut slot = DeviceSlot::new(
            slot_id,
            port_num,
            speed,
            mps,
            icc_buf.phys,
            icc_buf.virt,
            ep0_ring,
            address,
        );

        get_device_descriptor(&mut slot, doorbell_va, icc_buf.phys, icc_buf.virt)?;

        if bsr {
            let desc_mps_raw = unsafe { core::ptr::read_volatile((icc_buf.virt + 7) as *const u8) };
            let desc_mps = if desc_mps_raw < 8 { 8 } else { desc_mps_raw as u16 };
            let icc2 = unsafe { &mut *(icc_buf.virt as *mut InputControlContext) };
            *icc2 = InputControlContext::new_slot();
            context::init_icc_for_address_device(icc2, speed, port_num, desc_mps, slot.ep0_ring.phys);
            command::submit_address_device(cmd_ring, doorbell_va, icc_buf.phys, slot_id, false)?;
            slot.mps = desc_mps;
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
