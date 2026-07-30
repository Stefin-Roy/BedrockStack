use crate::services::serial::{SerialConsole, init as serial_init};

pub fn init() -> &'static dyn SerialConsole {
    serial_init()
}
