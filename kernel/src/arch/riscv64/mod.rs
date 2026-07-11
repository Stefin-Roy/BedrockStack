pub mod paging;
pub mod sbi;
pub mod serial;
pub mod time;
pub mod trap;

pub struct Riscv64;

use core::arch::asm;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::KernelLayout;
use super::Arch;

impl Arch for Riscv64 {
    fn init() {
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: trap handler\n");
        trap::init();
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: enabling supervisor interrupts\n");
        unsafe {
            asm!("csrw sie, {}", in(reg) trap::MIE_SEIE | trap::MIE_SSIE);
        }
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init done\n");
    }

    fn halt() {
        unsafe { asm!("wfi"); }
    }

    fn disable_interrupts() {
        unsafe { asm!("csrci sstatus, 2"); }
    }

    fn enable_interrupts() {
        unsafe { asm!("csrsi sstatus, 2"); }
    }

    fn setup_virt_mem(
        allocator: &mut BitmapAllocator,
        layout: &KernelLayout,
        stack_guard: u64,
        fb_addr: u64,
        fb_height: usize,
        fb_stride: usize,
    ) {
        paging::setup(allocator, layout, stack_guard, fb_addr, fb_height, fb_stride);
    }
}
