use crate::acpi::gas::{gas_read16, gas_write16};
use crate::acpi::platform::{AcpiError, Gas, Pm1ControlBit};

pub struct Pm1ControlRegisters {
    pub pm1a: Gas,
    pub pm1b: Option<Gas>,
}

impl Pm1ControlRegisters {
    pub fn new(pm1a: Gas, pm1b: Option<Gas>) -> Self {
        Self { pm1a, pm1b }
    }

    pub fn set_sleep_typ(&self, typ: u8) -> Result<(), ()> {
        const SLP_TYP_MASK: u16 = 0x1C00;
        let val = gas_read16(&self.pm1a);
        gas_write16(&self.pm1a, (val & !SLP_TYP_MASK) | ((typ as u16) << 10));
        if let Some(ref pm1b) = self.pm1b {
            let valb = gas_read16(pm1b);
            gas_write16(pm1b, (valb & !SLP_TYP_MASK) | ((typ as u16) << 10));
        }
        Ok(())
    }

    pub fn set_bit(&self, bit: Pm1ControlBit, set: bool) -> Result<(), ()> {
        let mask = match bit {
            Pm1ControlBit::SleepEnable => 1 << 13,
        };
        let mut val = gas_read16(&self.pm1a);
        if set { val |= mask; } else { val &= !mask; }
        gas_write16(&self.pm1a, val);
        if let Some(ref pm1b) = self.pm1b {
            let mut valb = gas_read16(pm1b);
            if set { valb |= mask; } else { valb &= !mask; }
            gas_write16(pm1b, valb);
        }
        Ok(())
    }
}

pub struct FadtFields {
    pub pm1_control: Pm1ControlRegisters,
    pub reset_gas: Option<Gas>,
    pub reset_value: u8,
    pub reset_supported: bool,
}

fn r8(buf: &[u8], off: usize) -> u8 { buf[off] }
fn r32(buf: &[u8], off: usize) -> u32 { u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) }
fn r64(buf: &[u8], off: usize) -> u64 { u64::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3], buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]) }

fn read_gas(buf: &[u8], off: usize) -> Gas {
    Gas {
        address_space_id: r8(buf, off),
        register_bit_width: r8(buf, off + 1),
        register_bit_offset: r8(buf, off + 2),
        access_size: r8(buf, off + 3),
        address: r64(buf, off + 4),
    }
}

/// Parse FADT fields from `vaddr` (mapped table).
///
/// The FADT grew over ACPI revisions:
///   Rev 1 (ACPI 1.0): 116 bytes  — old u32 fields only
///   Rev 2 (ACPI 2.0): 132 bytes  — adds reset-reg, X_PM1a, X_PM1b at ≥ 116
///   Rev 3 (ACPI 3.0): 244 bytes
///   Rev 4+ (ACPI 4.0+): further grows
///
/// This parser checks `length` to avoid out-of-bounds access on a rev-1
/// table.
pub fn parse_fadt(vaddr: u64, length: u32) -> Result<FadtFields, AcpiError> {
    let len = length as usize;
    let raw = unsafe { core::slice::from_raw_parts(vaddr as *const u8, len) };

    if raw[0..4] != [b'F', b'A', b'C', b'P'] {
        return Err(AcpiError::BadSignature);
    }

    // FLAGS at offset 112 (valid for all revisions)
    let flags = if len >= 116 { r32(raw, 112) } else { 0 };

    // RESET_REG GAS at offset 116, RESET_VALUE at 128 (ACPI 2.0+, rev ≥ 2)
    let (reset_gas, reset_value, reset_supported) = if len >= 132 {
        let g = read_gas(raw, 116);
        let v = r8(raw, 128);
        (if g.address != 0 { Some(g) } else { None }, v, (flags & (1 << 10)) != 0)
    } else {
        (None, 0, false)
    };

    // PM1a control block: try X_PM1a_CNT_BLK (GAS at offset 172, ACPI 2.0+)
    // first, fallback to old u32 PM1a_CNT_BLK at offset 64.
    let pm1a_gas = if len >= 244 {
        let x = read_gas(raw, 172);
        if x.address != 0 {
            x
        } else {
            let old_addr = r32(raw, 64);
            Gas {
                address_space_id: 1, // IO port
                register_bit_width: 16,
                register_bit_offset: 0,
                access_size: 2,
                address: old_addr as u64,
            }
        }
    } else {
        let old_addr = r32(raw, 64);
        Gas {
            address_space_id: 1,
            register_bit_width: 16,
            register_bit_offset: 0,
            access_size: 2,
            address: old_addr as u64,
        }
    };

    let pm1b_gas = if len >= 244 {
        let x = read_gas(raw, 184);
        if x.address != 0 {
            Some(x)
        } else {
            let old_addr = r32(raw, 68);
            if old_addr != 0 {
                Some(Gas {
                    address_space_id: 1,
                    register_bit_width: 16,
                    register_bit_offset: 0,
                    access_size: 2,
                    address: old_addr as u64,
                })
            } else {
                None
            }
        }
    } else {
        let old_addr = r32(raw, 68);
        if old_addr != 0 {
            Some(Gas {
                address_space_id: 1,
                register_bit_width: 16,
                register_bit_offset: 0,
                access_size: 2,
                address: old_addr as u64,
            })
        } else {
            None
        }
    };

    let pm1_control = Pm1ControlRegisters::new(pm1a_gas, pm1b_gas);

    Ok(FadtFields {
        pm1_control,
        reset_gas,
        reset_value,
        reset_supported,
    })
}
