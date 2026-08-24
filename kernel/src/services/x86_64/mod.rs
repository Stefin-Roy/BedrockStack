pub mod x86_acpi;
pub mod x86_cpu;
pub mod x86_interrupts;
pub mod x86_msi;
pub mod x86_pci_device;
pub mod x86_platform;
pub mod x86_serial;

use crate::acpi::AcpiSubsystem;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::services::KernelServices;
use crate::services::acpi::AcpiProvider;

pub fn x86_services(
    root: u64,
    alloc: *mut BitmapAllocator,
    acpi: Option<&'static AcpiSubsystem>,
) -> KernelServices {
    let acpi_provider: Option<&'static dyn AcpiProvider> = acpi.map(|a| {
        // leak the X86Acpi so it has a 'static lifetime
        let boxed = alloc::boxed::Box::new(x86_acpi::X86Acpi::new(a));
        &*alloc::boxed::Box::leak(boxed) as &'static dyn AcpiProvider
    });

    KernelServices {
        timer: crate::services::universal_timer::universal_timer(),
        interrupts: x86_interrupts::init(),
        serial: x86_serial::init(),
        platform: x86_platform::init(),
        cpu: x86_cpu::init(),
        pci_cfg: crate::services::ecam_pci_config::init(),
        msi: x86_msi::init(),
        pci: x86_pci_device::init(),
        acpi: acpi_provider,
        dma: crate::services::dma::init_dma_allocator(root, alloc),
        random: crate::services::random::init(),
    }
}
