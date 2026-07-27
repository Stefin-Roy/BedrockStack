use alloc::vec::Vec;
use crate::drivers::serial::SerialPort;
use crate::usb::dma::UsbDmaAllocator;
use crate::usb::usb;

pub struct UsbDevice {
    pub slot_id: u8,
    pub port_num: u8,
    pub speed: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_usb: u16,
    pub bcd_device: u16,
    pub max_packet_size0: u16,
    pub num_configs: u8,
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
        UsbDevice {
            slot_id, port_num, speed,
            address: 0,
            vendor_id: 0, product_id: 0,
            bcd_usb: 0, bcd_device: 0,
            max_packet_size0: max_pkt,
            num_configs: 0,
        }
    }
}

pub struct UsbDeviceManager {
    pub devices: Vec<UsbDevice>,
    next_address: u8,
}

impl UsbDeviceManager {
    pub fn new() -> Self {
        UsbDeviceManager {
            devices: Vec::new(),
            next_address: 1,
        }
    }

    pub fn enumerate_port(
        &mut self,
        _dma: &mut UsbDmaAllocator,
        port_num: u8,
        speed: u8,
    ) -> Result<(), &'static str> {
        let slot_id = (self.devices.len() + 1) as u8;
        if slot_id > 31 {
            return Err("max slots reached");
        }
        let dev_addr = self.next_address;
        self.next_address += 1;

        let mut dev = UsbDevice::new(slot_id, port_num, speed);
        dev.address = dev_addr;

        SerialPort::puts("[xhci] dev ");
        SerialPort::put_u64(slot_id as u64);
        SerialPort::puts(": slot=");
        SerialPort::put_u64(slot_id as u64);
        SerialPort::puts(" addr=");
        SerialPort::put_u64(dev_addr as u64);
        SerialPort::puts(" port=");
        SerialPort::put_u64(port_num as u64);
        SerialPort::puts(" speed=");
        SerialPort::put_u64(speed as u64);
        SerialPort::puts(" mps=");
        SerialPort::put_u64(dev.max_packet_size0 as u64);
        SerialPort::puts("\n");

        self.devices.push(dev);
        Ok(())
    }
}

pub fn allocate_dcbaa(dma: &mut UsbDmaAllocator, max_slots: u8) -> Result<super::DmaBuffer, &'static str> {
    let bytes = (max_slots as usize + 1) * 8;
    let pages = (bytes + 4095) / 4096;
    let buf = dma.alloc_contiguous(pages).ok_or("OOM for DCBAA")?;
    unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, buf.size) };
    Ok(buf)
}
