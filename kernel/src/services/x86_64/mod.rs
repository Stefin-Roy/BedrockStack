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

    // IOMMU policy: always-on unless kernel cmdline contains `noiommu`
    // (or `-noiommu`). Opt-out, not opt-in — `is_noiommu()` is the single
    // gate. When DMAR is absent and `noiommu` is not set, DMA remains
    // unprotected but a loud warning is emitted so the misconfiguration is
    // visible (hardware/firmware lacks reporting, not a silent fallback).
    let dma: &'static dyn crate::services::dma::DmaAllocator = {
        if crate::bootargs::is_noiommu() {
            crate::drivers::serial::SerialPort::puts("[svc] IOMMU disabled via noiommu\n");
            crate::services::dma::init_dma_allocator(root, alloc)
        } else if let Some(subsys) = acpi {
            if let Some(ref dmar) = subsys.dmar {
                let ok = crate::iommu::init(dmar, root, alloc);
                if ok {
                    crate::drivers::serial::SerialPort::puts("[svc] IOMMU enabled\n");
                    let iommu_dma = alloc::boxed::Box::new(crate::iommu::dma_remap::IommuDma::new(
                        root, alloc,
                    ));
                    let leaked: &'static crate::iommu::dma_remap::IommuDma =
                        alloc::boxed::Box::leak(iommu_dma);
                    crate::iommu::dma_remap::set_global(leaked);
                    // Program fault MSI to vector 53 on the BSP (interrupt is always reserved in IDT).
                    // Best-effort: read BSP APIC id after LAPIC is live.
                    {
                        let bsp = crate::platform::x86_64_pc::apic::read_full_apic_id();
                        crate::iommu::program_fault_msi(crate::arch::x86_64::idt::IOMMU_FAULT_VECTOR, bsp);
                    }
                    leaked as &'static dyn crate::services::dma::DmaAllocator
                } else {
                    crate::drivers::serial::SerialPort::puts("[svc] IOMMU init failed — DMA UNPROTECTED (use noiommu to silence)\n");
                    crate::services::dma::init_dma_allocator(root, alloc)
                }
            } else {
                crate::drivers::serial::SerialPort::puts("[svc] DMAR absent — DMA UNPROTECTED (firmware lacks VT-d / QEMU needs -device intel-iommu)\n");
                crate::services::dma::init_dma_allocator(root, alloc)
            }
        } else {
            crate::drivers::serial::SerialPort::puts("[svc] ACPI absent — DMA UNPROTECTED\n");
            crate::services::dma::init_dma_allocator(root, alloc)
        }
    };

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
        dma,
        random: crate::services::random::init(),
    }
}
