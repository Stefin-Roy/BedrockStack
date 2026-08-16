pub mod riscv_acpi;
pub mod riscv_cpu;
pub mod riscv_interrupts;
pub mod riscv_pci_device;
pub mod riscv_platform;

use crate::acpi::AcpiSubsystem;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::services::KernelServices;

pub fn riscv_services(
    root: u64,
    alloc: *mut BitmapAllocator,
    _acpi: Option<&'static AcpiSubsystem>,
) -> KernelServices {
    KernelServices {
        timer: crate::services::universal_timer::universal_timer(),
        interrupts: riscv_interrupts::init(),
        serial: crate::services::serial::init(),
        platform: riscv_platform::init(),
        cpu: riscv_cpu::init(),
        pci_cfg: crate::services::ecam_pci_config::init(),
        msi: crate::services::null_msi::init(),
        pci: riscv_pci_device::init(),
        acpi: Some(riscv_acpi::init()),
        dma: crate::services::dma::init_dma_allocator(root, alloc),
    }
}
