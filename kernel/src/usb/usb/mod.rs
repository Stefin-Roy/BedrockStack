pub mod descriptors;

pub const USB_DIR_OUT: u8 = 0;
pub const USB_DIR_IN: u8 = 1;

pub const USB_TYPE_STANDARD: u8 = 0;
pub const USB_TYPE_CLASS: u8 = 1;
pub const USB_TYPE_VENDOR: u8 = 2;

pub const USB_RECIP_DEVICE: u8 = 0;
pub const USB_RECIP_INTERFACE: u8 = 1;
pub const USB_RECIP_ENDPOINT: u8 = 2;
pub const USB_RECIP_OTHER: u8 = 3;

pub const BMREQ_DEVICE_TO_HOST: u8 = 1 << 7;
pub const BMREQ_HOST_TO_DEVICE: u8 = 0;

pub const REQ_GET_DESCRIPTOR: u8 = 6;
pub const REQ_SET_ADDRESS: u8 = 5;
pub const REQ_SET_CONFIGURATION: u8 = 9;
pub const REQ_GET_CONFIGURATION: u8 = 8;
pub const REQ_GET_INTERFACE: u8 = 10;
pub const REQ_SET_INTERFACE: u8 = 11;
pub const REQ_CLEAR_FEATURE: u8 = 1;
pub const REQ_SET_FEATURE: u8 = 3;

pub const DESC_DEVICE: u8 = 1;
pub const DESC_CONFIG: u8 = 2;
pub const DESC_STRING: u8 = 3;
pub const DESC_INTERFACE: u8 = 4;
pub const DESC_ENDPOINT: u8 = 5;
pub const DESC_DEVICE_QUALIFIER: u8 = 6;
pub const DESC_OTHER_SPEED_CONFIG: u8 = 7;
pub const DESC_INTERFACE_POWER: u8 = 8;
pub const DESC_BOS: u8 = 15;
pub const DESC_SS_EP_COMPANION: u8 = 48;
pub const DESC_SS_ISOCH_EP_COMPANION: u8 = 49;

pub const CLASS_HUB: u8 = 9;
pub const CLASS_MASS_STORAGE: u8 = 8;
pub const CLASS_HID: u8 = 3;

pub const SPEED_LS: u8 = 1;
pub const SPEED_FS: u8 = 2;
pub const SPEED_HS: u8 = 3;
pub const SPEED_SS: u8 = 4;

pub const EP_TYPE_CONTROL: u8 = 0;
pub const EP_TYPE_ISOCH: u8 = 1;
pub const EP_TYPE_BULK: u8 = 2;
pub const EP_TYPE_INTERRUPT: u8 = 3;

pub const FEATURE_ENDPOINT_HALT: u8 = 0;
pub const FEATURE_DEVICE_REMOTE_WAKEUP: u8 = 1;
pub const FEATURE_TEST_MODE: u8 = 2;
pub const FEATURE_B_DEVICE_HNP_ENABLE: u8 = 3;
pub const FEATURE_A_DEVICE_HNP_SUPPORT: u8 = 4;
pub const FEATURE_A_ALT_HNP_SUPPORT: u8 = 5;

pub const PORT_CONNECTION: u16 = 0;
pub const PORT_ENABLE: u16 = 1;
pub const PORT_SUSPEND: u16 = 2;
pub const PORT_OVER_CURRENT: u16 = 3;
pub const PORT_RESET: u16 = 4;
pub const PORT_POWER: u16 = 8;
pub const PORT_LOW_SPEED: u16 = 9;
pub const PORT_HIGH_SPEED: u16 = 10;
pub const PORT_TEST: u16 = 11;
pub const PORT_INDICATOR: u16 = 12;

pub const SS_ATTRIBUTE_BUDIO_BIT: u8 = 1 << 5;
pub const SS_ATTRIBUTE_SSP: u8 = 1 << 6;

pub struct SetupPacket {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub w_value: u16,
    pub w_index: u16,
    pub w_length: u16,
}

impl SetupPacket {
    pub fn get_descriptor(desc_type: u8, desc_index: u8, lang_id: u16, len: u16) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_DEVICE_TO_HOST | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            b_request: REQ_GET_DESCRIPTOR,
            w_value: ((desc_type as u16) << 8) | desc_index as u16,
            w_index: lang_id,
            w_length: len,
        }
    }

    pub fn set_address(address: u8) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_HOST_TO_DEVICE | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            b_request: REQ_SET_ADDRESS,
            w_value: address as u16,
            w_index: 0,
            w_length: 0,
        }
    }

    pub fn set_configuration(config: u8) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_HOST_TO_DEVICE | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            b_request: REQ_SET_CONFIGURATION,
            w_value: config as u16,
            w_index: 0,
            w_length: 0,
        }
    }

    pub fn set_interface(interface: u16, alt_setting: u16) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_HOST_TO_DEVICE | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
            b_request: REQ_SET_INTERFACE,
            w_value: alt_setting,
            w_index: interface,
            w_length: 0,
        }
    }

    pub fn get_interface(interface: u16, len: u16) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_DEVICE_TO_HOST | USB_TYPE_STANDARD | USB_RECIP_INTERFACE,
            b_request: REQ_GET_INTERFACE,
            w_value: 0,
            w_index: interface,
            w_length: len,
        }
    }

    pub fn clear_feature(recip: u8, feature: u8, index: u16) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_HOST_TO_DEVICE | USB_TYPE_STANDARD | recip,
            b_request: REQ_CLEAR_FEATURE,
            w_value: feature as u16,
            w_index: index,
            w_length: 0,
        }
    }

    pub fn set_feature(recip: u8, feature: u8, index: u16) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_HOST_TO_DEVICE | USB_TYPE_STANDARD | recip,
            b_request: REQ_SET_FEATURE,
            w_value: feature as u16,
            w_index: index,
            w_length: 0,
        }
    }

    pub fn get_configuration(len: u16) -> Self {
        SetupPacket {
            bm_request_type: BMREQ_DEVICE_TO_HOST | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
            b_request: REQ_GET_CONFIGURATION,
            w_value: 0,
            w_index: 0,
            w_length: len,
        }
    }
}
