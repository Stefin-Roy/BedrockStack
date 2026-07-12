pub mod paging;
pub mod sbi;
pub mod serial;
pub mod time;
pub mod trampoline;
pub mod trap;

pub struct Riscv64;

use core::arch::asm;
use crate::acpi::AcpiSubsystem;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::platform::riscv_virt::plic;
use crate::smp::ApContext;
use crate::KernelLayout;
use super::Arch;

impl Arch for Riscv64 {
    fn init() {
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: trap handler\n");
        trap::init();
        // Set BSP's APIC/hart ID before PLIC init so scontext() can read it.
        crate::smp::set_bsp_hardware_id(
            crate::platform::riscv_virt::plic::HART_ID.load(core::sync::atomic::Ordering::Relaxed) as u32
        );
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: PLIC\n");
        plic::init();
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: enabling supervisor interrupts\n");
        unsafe {
            asm!("csrw sie, {}", in(reg) trap::MIE_SEIE | trap::MIE_SSIE | trap::MIE_STIE);
        }
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init done\n");
    }

    fn init_ap(_cpu_id: u32) {
        // Set up trap vector for this hart.
        trap::init();
        // S-mode interrupt enable in sie.
        unsafe {
            asm!("csrw sie, {}", in(reg) trap::MIE_SEIE | trap::MIE_SSIE | trap::MIE_STIE);
        }
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
    ) -> Vmm {
        paging::setup(allocator, layout, stack_guard, fb_addr, fb_height, fb_stride)
    }

    fn discover_cpus(acpi: Option<&AcpiSubsystem>) -> alloc::vec::Vec<(u32, bool)> {
        // First try DTB (passed via a global set by the boot code).
        if let Some(dtb) = crate::platform::riscv_virt::get_dtb_ptr() {
            let cpus = crate::dtb::parse_cpus(dtb);
            if !cpus.is_empty() {
                return cpus;
            }
        }
        // Fall back to ACPI MADT if available.
        if let Some(ref acpi) = acpi {
            if let Some(ref pi) = acpi.platform.processor_info {
                let mut cpus = alloc::vec::Vec::new();
                cpus.push((pi.boot_processor.local_apic_id as u32, true));
                for proc in &pi.application_processors {
                    let enabled = proc.state != ::acpi::platform::ProcessorState::Disabled;
                    cpus.push((proc.local_apic_id as u32, enabled));
                }
                return cpus;
            }
        }
        alloc::vec::Vec::new()
    }

    unsafe fn wake_aps(
        allocator: &mut BitmapAllocator,
        page_table_root: u64,
        aps: &[ApContext],
    ) -> usize {
        unsafe { trampoline::start_aps(allocator, page_table_root, aps) }
    }
}
