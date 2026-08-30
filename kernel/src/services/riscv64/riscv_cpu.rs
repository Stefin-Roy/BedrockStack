use alloc::vec::Vec;

use super::super::cpu::CpuManager;
use crate::acpi::AcpiSubsystem;
use crate::arch::riscv64::sbi;
use crate::smp::ApContext;
use crate::smp::CpuInfo;

pub struct RiscvCpu;

impl CpuManager for RiscvCpu {
    fn current_cpu_id(&self) -> u32 {
        crate::smp::current_cpu_id()
    }

    fn cpu_count(&self) -> u32 {
        crate::smp::cpu_count()
    }

    fn send_ipi(&self, _cpu_id: u32, _vector: u8) {
        let hart_mask = 1u64 << _cpu_id;
        sbi::send_ipi(hart_mask);
    }

    fn broadcast_ipi_except_self(&self, _vector: u8) {
        let self_id = self.current_cpu_id();
        let count = self.cpu_count();
        let mut mask = 0u64;
        for i in 0..count {
            if i != self_id {
                mask |= 1u64 << i;
            }
        }
        sbi::send_ipi(mask);
    }

    fn discover_cpus(&self, acpi: Option<&AcpiSubsystem>) -> Vec<CpuInfo> {
        if let Some(dtb) = crate::platform::riscv_virt::get_dtb_ptr() {
            let cpus = crate::dtb::parse_cpus(dtb);
            if !cpus.is_empty() {
                return cpus
                    .into_iter()
                    .map(|(hardware_id, enabled)| CpuInfo { hardware_id, enabled })
                    .collect();
            }
        }
        if let Some(ref acpi) = acpi {
            if !acpi.cpus.is_empty() {
                return acpi
                    .cpus
                    .iter()
                    .map(|&(hardware_id, enabled)| CpuInfo { hardware_id, enabled })
                    .collect();
            }
        }
        Vec::new()
    }

    unsafe fn wake_aps(&self, page_table_root: u64, aps: &[ApContext]) -> usize {
        let alloc = crate::mm::heap::get_phys_allocator_mut();
        unsafe { crate::arch::riscv64::trampoline::start_aps(alloc, page_table_root, aps) }
    }
}

static RISCV_CPU: RiscvCpu = RiscvCpu;

pub fn init() -> &'static dyn CpuManager {
    &RISCV_CPU as &'static dyn CpuManager
}
