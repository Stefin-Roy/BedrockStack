use crate::drivers::serial::SerialPort;
use log::{Metadata, Record};

/// Routes `log` crate output to the kernel serial port.
pub(crate) struct AcpiLogger;

impl log::Log for AcpiLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            SerialPort::puts("[acpi] ");
            SerialPort::puts(record.level().as_str());
            SerialPort::puts(": ");
            if let Some(s) = record.args().as_str() {
                SerialPort::puts(s);
            }
            SerialPort::puts("\n");
        }
    }

    fn flush(&self) {}
}

static LOGGER: AcpiLogger = AcpiLogger;

/// Initialise the ACPI logger (called once during kernel startup).
pub(crate) fn init() {
    log::set_logger(&LOGGER).ok();
    log::set_max_level(log::LevelFilter::Debug);
}
