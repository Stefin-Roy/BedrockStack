pub mod capability;
pub mod clocksource;
pub mod clockevent;
pub mod timer_queue;
pub mod universal_timer;
pub mod interrupts;
pub mod phys_mem;
pub mod virt_mem;
pub mod serial;
pub mod platform;
pub mod cpu;
pub mod pci_config;
pub mod ecam_pci_config;
pub mod msi;
pub mod null_msi;
pub mod dma;
pub mod block_device;
pub mod pci_device;
pub mod acpi;
pub mod wallclock;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "riscv64")]
pub mod riscv64;

use crate::acpi::AcpiSubsystem;
use crate::mm::phys_alloc::BitmapAllocator;

use universal_timer::UniversalTimer;
use interrupts::InterruptManager;
use serial::SerialConsole;
use platform::PlatformControl;
use cpu::CpuManager;
use pci_config::PciConfigSpace;
use msi::MsiAllocator;
use dma::DmaAllocator;
use pci_device::PciDeviceManager;
use acpi::AcpiProvider;

/// Container for all arch-independent kernel capability providers.
///
/// Built once during `Kernel::init()` after arch init completes.
/// Accessed via `Kernel::svc()` throughout the rest of the boot sequence.
pub struct KernelServices {
    pub timer: &'static dyn UniversalTimer,
    pub interrupts: &'static dyn InterruptManager,
    pub serial: &'static dyn SerialConsole,
    pub platform: &'static dyn PlatformControl,
    pub cpu: &'static dyn CpuManager,
    pub pci_cfg: &'static dyn PciConfigSpace,
    pub msi: &'static dyn MsiAllocator,
    pub pci: &'static dyn PciDeviceManager,
    pub acpi: Option<&'static dyn AcpiProvider>,
    pub dma: &'static dyn DmaAllocator,
}

/// Build the platform-appropriate service container.
///
/// Platform selection is confined to this single function. Every consumer
/// accesses services via `dyn` dispatch — no `#[cfg(target_arch)]` needed
/// outside this module.
pub fn init_services(
    root: u64,
    alloc: *mut BitmapAllocator,
    acpi: Option<&'static AcpiSubsystem>,
) -> KernelServices {
    #[cfg(target_arch = "x86_64")]
    return x86_64::x86_services(root, alloc, acpi);

    #[cfg(target_arch = "riscv64")]
    return riscv64::riscv_services(root, alloc, acpi);
}
