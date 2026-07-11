use common::serial::IoBackend;

/// MMIO-based I/O backend for a 16550 UART on RISC-V.
///
/// The QEMU virt platform maps the UART at 0x10000000.
pub struct MmioIo;

impl IoBackend for MmioIo {
    fn read_reg(offset: u16) -> u8 {
        // TODO: implement MMIO read
        // let addr = (UART_BASE + offset as u64) as *const u8;
        // unsafe { addr.read_volatile() }
        0
    }

    fn write_reg(offset: u16, _val: u8) {
        // TODO: implement MMIO write
        // let addr = (UART_BASE + offset as u64) as *mut u8;
        // unsafe { addr.write_volatile(val) }
    }
}

/// Concrete serial port type for RISC-V (MMIO at 0x10000000).
pub type SerialPort = common::serial::SerialPort<MmioIo>;
