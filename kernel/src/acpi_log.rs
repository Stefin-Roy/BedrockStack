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
            let msg = record.args().as_str().unwrap_or("");
            if !msg.is_empty() {
                SerialPort::puts("[acpi] ");
                SerialPort::puts(record.level().as_str());
                SerialPort::puts(": ");
                SerialPort::puts(msg);
                SerialPort::puts("\n");
            } else {
                // Dynamic format args — use fmt::Write to render them.
                SerialPort::puts("[acpi] ");
                SerialPort::puts(record.level().as_str());
                SerialPort::puts(": ");
                use core::fmt::Write;
                let _ = write!(SerialPort::new(), "{}", record.args());
                SerialPort::puts("\n");
            }
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
