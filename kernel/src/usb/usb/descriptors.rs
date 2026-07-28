use core::mem::size_of;

#[repr(C, packed)]
pub struct DeviceDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub bcd_usb: u16,
    pub b_device_class: u8,
    pub b_device_subclass: u8,
    pub b_device_protocol: u8,
    pub b_max_packet_size0: u8,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub i_manufacturer: u8,
    pub i_product: u8,
    pub i_serial_number: u8,
    pub b_num_configurations: u8,
}

impl DeviceDescriptor {
    pub fn parse(data: &[u8]) -> Option<&DeviceDescriptor> {
        if data.len() < size_of::<DeviceDescriptor>() {
            return None;
        }
        if data[0] < size_of::<DeviceDescriptor>() as u8 {
            return None;
        }
        if data[1] != super::DESC_DEVICE {
            return None;
        }
        Some(unsafe { &*(data.as_ptr() as *const DeviceDescriptor) })
    }
}

#[repr(C, packed)]
pub struct ConfigDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub w_total_length: u16,
    pub b_num_interfaces: u8,
    pub b_configuration_value: u8,
    pub i_configuration: u8,
    pub bm_attributes: u8,
    pub b_max_power: u8,
}

impl ConfigDescriptor {
    pub fn parse(data: &[u8]) -> Option<&ConfigDescriptor> {
        if data.len() < size_of::<ConfigDescriptor>() {
            return None;
        }
        if data[0] < size_of::<ConfigDescriptor>() as u8 {
            return None;
        }
        if data[1] != super::DESC_CONFIG {
            return None;
        }
        Some(unsafe { &*(data.as_ptr() as *const ConfigDescriptor) })
    }

    pub fn total_length(&self) -> u16 {
        u16::from_le(unsafe { core::ptr::addr_of!(self.w_total_length).read_unaligned() })
    }

    pub fn num_interfaces(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_num_interfaces).read_unaligned() }
    }

    pub fn configuration_value(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_configuration_value).read_unaligned() }
    }

    pub fn attributes(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.bm_attributes).read_unaligned() }
    }

    pub fn max_power(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_max_power).read_unaligned() }
    }
}

#[repr(C, packed)]
pub struct InterfaceDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_interface_number: u8,
    pub b_alternate_setting: u8,
    pub b_num_endpoints: u8,
    pub b_interface_class: u8,
    pub b_interface_subclass: u8,
    pub b_interface_protocol: u8,
    pub i_interface: u8,
}

impl InterfaceDescriptor {
    pub fn parse(data: &[u8]) -> Option<&InterfaceDescriptor> {
        if data.len() < size_of::<InterfaceDescriptor>() {
            return None;
        }
        if data[0] < size_of::<InterfaceDescriptor>() as u8 {
            return None;
        }
        if data[1] != super::DESC_INTERFACE {
            return None;
        }
        Some(unsafe { &*(data.as_ptr() as *const InterfaceDescriptor) })
    }

    pub fn interface_number(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_interface_number).read_unaligned() }
    }

    pub fn alternate_setting(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_alternate_setting).read_unaligned() }
    }

    pub fn num_endpoints(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_num_endpoints).read_unaligned() }
    }

    pub fn class(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_interface_class).read_unaligned() }
    }

    pub fn subclass(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_interface_subclass).read_unaligned() }
    }

    pub fn protocol(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_interface_protocol).read_unaligned() }
    }
}

#[repr(C, packed)]
pub struct EndpointDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_endpoint_address: u8,
    pub bm_attributes: u8,
    pub w_max_packet_size: u16,
    pub b_interval: u8,
}

impl EndpointDescriptor {
    pub fn parse(data: &[u8]) -> Option<&EndpointDescriptor> {
        if data.len() < size_of::<EndpointDescriptor>() {
            return None;
        }
        if data[0] < size_of::<EndpointDescriptor>() as u8 {
            return None;
        }
        if data[1] != super::DESC_ENDPOINT {
            return None;
        }
        Some(unsafe { &*(data.as_ptr() as *const EndpointDescriptor) })
    }

    pub fn endpoint_number(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_endpoint_address).read_unaligned() & 0x0F }
    }

    pub fn is_in(&self) -> bool {
        unsafe { core::ptr::addr_of!(self.b_endpoint_address).read_unaligned() & 0x80 != 0 }
    }

    pub fn transfer_type(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.bm_attributes).read_unaligned() & 0x03 }
    }

    pub fn max_packet_size(&self) -> u16 {
        u16::from_le(unsafe { core::ptr::addr_of!(self.w_max_packet_size).read_unaligned() }) & 0x07FF
    }

    pub fn interval(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_interval).read_unaligned() }
    }
}

#[repr(C, packed)]
pub struct SsEndpointCompanionDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_max_burst: u8,
    pub bm_attributes: u8,
    pub w_bytes_per_interval: u16,
}

impl SsEndpointCompanionDescriptor {
    pub fn parse(data: &[u8]) -> Option<&SsEndpointCompanionDescriptor> {
        if data.len() < size_of::<SsEndpointCompanionDescriptor>() {
            return None;
        }
        if data[0] < size_of::<SsEndpointCompanionDescriptor>() as u8 {
            return None;
        }
        if data[1] != super::DESC_SS_EP_COMPANION {
            return None;
        }
        Some(unsafe { &*(data.as_ptr() as *const SsEndpointCompanionDescriptor) })
    }

    pub fn max_burst(&self) -> u8 {
        unsafe { core::ptr::addr_of!(self.b_max_burst).read_unaligned() }
    }

    pub fn bytes_per_interval(&self) -> u16 {
        u16::from_le(unsafe { core::ptr::addr_of!(self.w_bytes_per_interval).read_unaligned() })
    }
}

pub fn parse_config_descriptors(data: &[u8]) {
    // Determine the total configuration length from the config descriptor header.
    let total_len = if data.len() >= size_of::<ConfigDescriptor>() {
        match ConfigDescriptor::parse(data) {
            Some(cfg) => cfg.total_length() as usize,
            None => return,
        }
    } else {
        return;
    };

    let limit = data.len().min(total_len);
    let mut offset = 0;
    while offset < limit {
        if offset + 2 > limit {
            break;
        }
        let len = data[offset] as usize;
        let desc_type = data[offset + 1];
        if len < 2 {
            break;
        }
        if offset + len > limit {
            break;
        }
        match desc_type {
            super::DESC_CONFIG => {
                let _ = ConfigDescriptor::parse(&data[offset..]);
            }
            super::DESC_INTERFACE => {
                let _ = InterfaceDescriptor::parse(&data[offset..]);
            }
            super::DESC_ENDPOINT => {
                let _ = EndpointDescriptor::parse(&data[offset..]);
            }
            super::DESC_SS_EP_COMPANION => {
                let _ = SsEndpointCompanionDescriptor::parse(&data[offset..]);
            }
            _ => {}
        }
        offset += len;
    }
}
