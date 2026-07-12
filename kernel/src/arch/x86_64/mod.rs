pub mod gdt;
pub mod idt;
pub mod paging;
pub mod serial;
pub mod trampoline;

use crate::acpi::AcpiSubsystem;
use crate::platform::x86_64_pc::apic;

pub struct X86_64;

use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::smp::ApContext;
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
        // Record the BSP's APIC ID after APIC init.
        crate::smp::set_bsp_hardware_id(apic::read_apic_id() as u32);
    }

    fn init_ap(_cpu_id: u32) {
        crate::arch::x86_64::gdt::init();
        crate::arch::x86_64::idt::init();
        crate::platform::x86_64_pc::apic::init_ap();
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
    ) -> Vmm {
        paging::setup(allocator, layout, stack_guard, fb_addr, fb_height, fb_stride)
    }

    fn discover_cpus(acpi: Option<&AcpiSubsystem>) -> alloc::vec::Vec<(u32, bool)> {
        let mut cpus = alloc::vec::Vec::new();

        let Some(ref processor_info) = acpi.and_then(|a| a.processor_info.as_ref()) else {
            SerialPort::puts("[arch] no processor info from ACPI\n");
            // At least report the BSP
            return cpus;
        };

        SerialPort::puts("[arch] boot processor: apic_id=");
        SerialPort::put_u64(processor_info.boot_processor.local_apic_id as u64);
        SerialPort::puts("\n");

        cpus.push((processor_info.boot_processor.local_apic_id as u32, true));

        for proc in &processor_info.application_processors {
            let enabled = proc.state != crate::acpi::ProcessorState::Disabled;
            if !enabled {
                SerialPort::puts("[arch] skipping disabled AP: apic_id=");
                SerialPort::put_u64(proc.local_apic_id as u64);
                SerialPort::puts("\n");
            }
            cpus.push((proc.local_apic_id as u32, enabled));
        }

        cpus
    }

    unsafe fn wake_aps(
        allocator: &mut BitmapAllocator,
        page_table_root: u64,
        aps: &[ApContext],
    ) -> usize {
        unsafe { trampoline::start_aps(allocator, page_table_root, aps) }
    }
}
