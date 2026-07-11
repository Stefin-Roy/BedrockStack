pub mod gdt;
pub mod idt;
pub mod paging;
pub mod serial;

use crate::platform::x86_64_pc::apic;

pub struct X86_64;

use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::KernelLayout;
use super::Arch;

impl Arch for X86_64 {
    fn init() {
        SerialPort::puts("[arch] x86_64 init: GDT\n");
        gdt::init();
        SerialPort::puts("[arch] x86_64 init: IDT\n");
        idt::init();
        SerialPort::puts("[arch] x86_64 init: APIC\n");
        apic::init();
    }

    fn halt() {
        x86_64::instructions::hlt();
    }

    fn disable_interrupts() {
        x86_64::instructions::interrupts::disable();
    }

    fn enable_interrupts() {
        x86_64::instructions::interrupts::enable();
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
