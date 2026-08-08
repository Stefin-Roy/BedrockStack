use crate::acpi::platform::{AcpiError, Gas};
use crate::mm::vmm::PageFlags;

fn map_phys(paddr: u64, size: u64) -> u64 {
    let offset = paddr & 0xFFF;
    let aligned = paddr - offset;
    let total = size + offset;
    let pages = (total + 0xFFF) & !0xFFF;
    let vaddr = crate::acpi::map_device_mmio(aligned, pages, PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE);
    vaddr + offset
}

fn mmio_read(addr: u64, width: u8) -> Result<u64, AcpiError> {
    unsafe {
        match width {
            8  => Ok((addr as *const u8).read_volatile() as u64),
            16 => Ok((addr as *const u16).read_volatile() as u64),
            32 => Ok((addr as *const u32).read_volatile() as u64),
            64 => Ok((addr as *const u64).read_volatile() as u64),
            _  => Err(AcpiError::Unsupported),
        }
    }
}

fn mmio_write(addr: u64, value: u64, width: u8) -> Result<(), AcpiError> {
    unsafe {
        match width {
            8  => (addr as *mut u8).write_volatile(value as u8),
            16 => (addr as *mut u16).write_volatile(value as u16),
            32 => (addr as *mut u32).write_volatile(value as u32),
            64 => (addr as *mut u64).write_volatile(value),
            _  => return Err(AcpiError::Unsupported),
        }
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn port_in(port: u16, width: u8) -> Result<u32, AcpiError> {
    unsafe {
        match width {
            8  => { let v: u8; core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags)); Ok(v as u32) }
            16 => { let v: u16; core::arch::asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags)); Ok(v as u32) }
            32 => { let v: u32; core::arch::asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack, preserves_flags)); Ok(v) }
            _  => Err(AcpiError::Unsupported),
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn port_out(port: u16, value: u32, width: u8) -> Result<(), AcpiError> {
    unsafe {
        match width {
            8  => core::arch::asm!("out dx, al", in("dx") port, in("al") value as u8, options(nomem, nostack, preserves_flags)),
            16 => core::arch::asm!("out dx, ax", in("dx") port, in("ax") value as u16, options(nomem, nostack, preserves_flags)),
            32 => core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)),
            _  => return Err(AcpiError::Unsupported),
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) fn port_in(_port: u16, width: u8) -> Result<u32, AcpiError> {
    if width == 8 || width == 16 || width == 32 {
        Ok(0)
    } else {
        Err(AcpiError::Unsupported)
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) fn port_out(_port: u16, _value: u32, width: u8) -> Result<(), AcpiError> {
    if width == 8 || width == 16 || width == 32 {
        Ok(())
    } else {
        Err(AcpiError::Unsupported)
    }
}

pub fn gas_read(gas: &Gas) -> Result<u64, AcpiError> {
    let width = if gas.register_bit_width > 0 { gas.register_bit_width } else { 16 };
    match gas.address_space_id {
        0 => {
            let size = ((width as u64 + 7) / 8).max(1);
            let vaddr = map_phys(gas.address, size);
            mmio_read(vaddr, width)
        }
        1 => port_in(gas.address as u16, width).map(|v| v as u64),
        _ => Err(AcpiError::Unsupported),
    }
}

pub fn gas_write(gas: &Gas, value: u64) -> Result<(), AcpiError> {
    let width = if gas.register_bit_width > 0 { gas.register_bit_width } else { 16 };
    match gas.address_space_id {
        0 => {
            let size = ((width as u64 + 7) / 8).max(1);
            let vaddr = map_phys(gas.address, size);
            mmio_write(vaddr, value, width)
        }
        1 => port_out(gas.address as u16, value as u32, width),
        _ => Err(AcpiError::Unsupported),
    }
}

pub fn gas_read16(gas: &Gas) -> Result<u16, AcpiError> {
    gas_read(gas).map(|v| v as u16)
}

pub fn gas_write16(gas: &Gas, value: u16) -> Result<(), AcpiError> {
    gas_write(gas, value as u64)
}
