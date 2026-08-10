use alloc::vec::Vec;

use crate::acpi::AcpiSubsystem;
use crate::platform::x86_64_pc::apic;
use crate::smp::ApContext;
use super::super::cpu::CpuManager;

pub struct X86Cpu;



impl CpuManager for X86Cpu {
    fn current_cpu_id(&self) -> u32 {
        crate::smp::current_cpu_id()
    }

    fn cpu_count(&self) -> u32 {
        crate::smp::cpu_count()
    }

    fn send_ipi(&self, cpu_id: u32, vector: u8) {
        apic::send_ipi(cpu_id, vector);
    }

    fn broadcast_ipi_except_self(&self, vector: u8) {
        apic::send_ipi_all_except_self(vector);
    }

    fn discover_cpus(&self, acpi: Option<&AcpiSubsystem>) -> Vec<(u32, bool)> {
        let Some(acpi) = acpi else {
            return Vec::new();
        };
        acpi.cpus.clone()
    }

    unsafe fn wake_aps(
        &self,
        page_table_root: u64,
        aps: &[ApContext],
    ) -> usize {
        let alloc = crate::mm::heap::get_phys_allocator_mut();
        unsafe {
            crate::arch::x86_64::trampoline::start_aps(alloc, page_table_root, aps)
        }
    }
}

static X86_CPU: X86Cpu = X86Cpu;

pub fn init() -> &'static dyn CpuManager {
    &X86_CPU as &'static dyn CpuManager
}
