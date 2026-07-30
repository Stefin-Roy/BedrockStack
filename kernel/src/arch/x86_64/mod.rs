pub mod gdt;
pub mod idt;
#[cfg(feature = "cpu_slow")]
pub mod limiter;
pub mod paging;
pub mod serial;
pub mod trampoline;
#[cfg(feature = "kernelmb2")]
mod multiboot2;

use crate::platform::x86_64_pc::apic;

pub struct X86_64;

use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::KernelLayout;

impl X86_64 {
    pub fn init() {
        SerialPort::puts("[arch] x86_64 init: GDT\n");
        gdt::init();
        SerialPort::puts("[arch] x86_64 init: IDT\n");
        idt::init();
        SerialPort::puts("[arch] x86_64 init: APIC\n");
        apic::init();
        // Record the BSP's APIC ID after APIC init.
        crate::smp::set_bsp_hardware_id(apic::read_full_apic_id());
    }

    pub fn init_ap(_cpu_id: u32) {
        crate::arch::x86_64::gdt::init();
        crate::arch::x86_64::idt::init_ap();
        crate::platform::x86_64_pc::apic::init_ap();
    }

    pub fn halt() {
        x86_64::instructions::hlt();
    }

    pub fn disable_interrupts() {
        x86_64::instructions::interrupts::disable();
    }

    pub fn enable_interrupts() {
        x86_64::instructions::interrupts::enable();
    }

    pub fn are_interrupts_enabled() -> bool {
        x86_64::instructions::interrupts::are_enabled()
    }

    pub fn setup_virt_mem(
        allocator: &mut BitmapAllocator,
        layout: &KernelLayout,
        stack_guard: u64,
        fb_addr: u64,
        fb_height: usize,
        fb_stride: usize,
        fb_bpp: u8,
    ) -> Vmm {
        paging::setup(allocator, layout, stack_guard, fb_addr, fb_height, fb_stride, fb_bpp)
    }


}
