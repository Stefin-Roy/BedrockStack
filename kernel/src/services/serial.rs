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
    // The serial console service delegates straight to the driver's raw
    // primitives; the capability indirection was removed in the de-cap, so
    // `SerialPort`'s public wrappers and this impl share the same raw path.
    fn putc(&self, c: u8) {
        crate::drivers::serial::raw_putc(c);
    }
    fn puts(&self, s: &str) {
        crate::drivers::serial::raw_puts(s);
    }
    fn put_hex(&self, val: u64) {
        crate::drivers::serial::raw_put_hex(val);
    }
    fn put_u64(&self, val: u64) {
        crate::drivers::serial::raw_put_u64(val);
    }
}

static KERNEL_SERIAL: KernelSerial = KernelSerial;

pub fn init() -> &'static dyn SerialConsole {
    &KERNEL_SERIAL as &'static dyn SerialConsole
}

/// C5: return the concrete serial node as a `'static` object for obj-endowment.
pub fn kernel_serial_static() -> &'static KernelSerial {
    &KERNEL_SERIAL
}
