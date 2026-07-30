use core::arch::asm;

use super::super::capability::Capability;
use super::super::platform::PlatformControl;

pub struct X86Platform;

impl Capability for X86Platform {
    fn name(&self) -> &str {
        "x86-platform"
    }
}

impl PlatformControl for X86Platform {
    fn shutdown(&self) -> ! {
        // Try ACPI S5 first (via PM1 control), fall back to QEMU port.
        // If ACPI is available, the caller should use AcpiSubsystem::shutdown().
        // Otherwise this minimal path tries the QEMU ISA debug port.
        let pm1a_port: u16 = 0x604;
        let val: u16 = (0x00u16 << 10) | (1u16 << 13);
        unsafe {
            asm!("out dx, ax", in("dx") pm1a_port, in("ax") val, options(nomem, nostack, preserves_flags));
        }
        loop { unsafe { asm!("cli; hlt", options(nomem, nostack)) } }
    }

    fn reset(&self) -> ! {
        // Try 8042 keyboard controller reset.
        unsafe {
            let mut status: u8;
            for _ in 0..100_000 {
                asm!("in al, dx", in("dx") 0x64u16, out("al") status, options(nomem, nostack, preserves_flags));
                if status & 0x02 == 0 {
                    break;
                }
            }
            asm!("out dx, al", in("dx") 0x64u16, in("al") 0xFEu8, options(nomem, nostack, preserves_flags));
        }
        loop { unsafe { asm!("cli; hlt", options(nomem, nostack)) } }
    }

    fn halt(&self) {
        x86_64::instructions::hlt();
    }

    fn disable_interrupts(&self) {
        x86_64::instructions::interrupts::disable();
    }

    fn enable_interrupts(&self) {
        x86_64::instructions::interrupts::enable();
    }

    fn are_interrupts_enabled(&self) -> bool {
        x86_64::instructions::interrupts::are_enabled()
    }
}

static X86_PLATFORM: X86Platform = X86Platform;

pub fn init() -> &'static dyn PlatformControl {
    &X86_PLATFORM as &'static dyn PlatformControl
}
