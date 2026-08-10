use core::arch::asm;

use crate::arch::riscv64::sbi;
use super::super::platform::PlatformControl;

pub struct RiscvPlatform;



impl PlatformControl for RiscvPlatform {
    fn shutdown(&self) -> ! {
        sbi::system_reset()
    }

    fn reset(&self) -> ! {
        sbi::cold_reboot()
    }

    fn halt(&self) {
        unsafe { asm!("wfi"); }
    }

    fn disable_interrupts(&self) {
        unsafe { asm!("csrci sstatus, 2"); }
    }

    fn enable_interrupts(&self) {
        unsafe { asm!("csrsi sstatus, 2"); }
    }

    fn are_interrupts_enabled(&self) -> bool {
        let stval: u64;
        unsafe { asm!("csrr {}, sstatus", out(reg) stval); }
        (stval & 2) != 0
    }
}

static RISCV_PLATFORM: RiscvPlatform = RiscvPlatform;

pub fn init() -> &'static dyn PlatformControl {
    &RISCV_PLATFORM as &'static dyn PlatformControl
}
