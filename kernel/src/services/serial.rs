
pub trait SerialConsole: Send + Sync {
    fn putc(&self, c: u8);
    fn puts(&self, s: &str);
    fn put_hex(&self, val: u64);
    fn put_u64(&self, val: u64);
}

/// Shared across both architectures — delegates to `drivers::serial::SerialPort`.
///
/// The arch-specific inner I/O (`PortIo` vs `MmioIo`) is hidden inside
/// `drivers::serial` via the `Inner` type alias.
pub struct KernelSerial;



impl SerialConsole for KernelSerial {
    fn putc(&self, c: u8) {
        crate::drivers::serial::SerialPort::putc(c);
    }
    fn puts(&self, s: &str) {
        crate::drivers::serial::SerialPort::puts(s);
    }
    fn put_hex(&self, val: u64) {
        crate::drivers::serial::SerialPort::put_hex(val);
    }
    fn put_u64(&self, val: u64) {
        crate::drivers::serial::SerialPort::put_u64(val);
    }
}

static KERNEL_SERIAL: KernelSerial = KernelSerial;

pub fn init() -> &'static dyn SerialConsole {
    &KERNEL_SERIAL as &'static dyn SerialConsole
}
