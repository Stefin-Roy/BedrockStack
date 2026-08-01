//! Module registry for static module dispatch.

use super::Module;
use super::fat32_ls::Fat32Ls;
use super::fat32_test::Fat32Test;
#[cfg(target_arch = "x86_64")]
use super::msix_test::MsixTest;
#[cfg(target_arch = "x86_64")]
use super::ps2_test::Ps2Test;
#[cfg(target_arch = "x86_64")]
use super::usb_test::UsbTest;
use super::vfs_test::VfsTest;
use framebuffer::Framebuffer;
use framebuffer::Display as _;

/// Hello World module.
struct HelloModule;

impl Module for HelloModule {
    fn name(&self) -> &str {
        "hello"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn init(&self, display: &mut Framebuffer) -> Result<(), &'static str> {
        let msg = b"Hello, World!";
        for (i, &ch) in msg.iter().enumerate() {
            display.draw_char(i * 8, 0, ch);
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
static MODULES: &[&dyn Module] = &[
    &HelloModule,
    &Fat32Test,
    &MsixTest,
    &UsbTest,
    &Fat32Ls,
    &VfsTest,
    &Ps2Test,
];

#[cfg(not(target_arch = "x86_64"))]
static MODULES: &[&dyn Module] = &[
    &HelloModule,
    &Fat32Test,
    &Fat32Ls,
    &VfsTest,
];

pub fn init_all(display: &mut Framebuffer) {
    for module in MODULES {
        match module.init(display) {
            Ok(()) => {}
            Err(msg) => {
                crate::drivers::serial::SerialPort::puts("[module] ");
                crate::drivers::serial::SerialPort::puts(module.name());
                crate::drivers::serial::SerialPort::puts(" init failed: ");
                crate::drivers::serial::SerialPort::puts(msg);
                crate::drivers::serial::SerialPort::puts("\n");
                break;
            }
        }
    }
}
