pub mod acpi;
pub mod block_device;
pub mod clockevent;
pub mod clocksource;
pub mod cpu;
pub mod dma;
pub mod ecam_pci_config;
pub mod interrupts;
pub mod msi;
pub mod null_msi;
pub mod pci_config;
pub mod pci_device;
pub mod phys_mem;
pub mod platform;
pub mod random;
pub mod serial;
pub mod timer_queue;
pub mod universal_timer;
pub mod virt_mem;
pub mod wallclock;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

use crate::acpi::AcpiSubsystem;
use crate::mm::phys_alloc::BitmapAllocator;

use acpi::AcpiProvider;
use cpu::CpuManager;
use dma::DmaAllocator;
use interrupts::InterruptManager;
use msi::MsiAllocator;
use pci_config::PciConfigSpace;
use pci_device::PciDeviceManager;
use platform::PlatformControl;
use random::Random;
use serial::SerialConsole;
use universal_timer::UniversalTimer;

/// Container for all arch-independent kernel service providers.
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
    pub random: &'static dyn Random,
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
    fb_range: Option<(u64, u64)>,
) -> KernelServices {
    #[cfg(target_arch = "x86_64")]
    return x86_64::x86_services(root, alloc, acpi, fb_range);

    #[cfg(target_arch = "riscv64")]
    return riscv64::riscv_services(root, alloc, acpi, fb_range);
}

// ── Global accessor for hot-path driver use ─────────────────────────

use spin::Once;

static GLOBAL_SERVICES: Once<&'static KernelServices> = Once::new();

/// Set the global services reference (called once after construction).
pub fn set_global(svc: &'static KernelServices) {
    GLOBAL_SERVICES.call_once(|| svc);
}

/// Returns the global `KernelServices` reference.
///
/// Panics if called before `set_global`.
pub fn kernel_services() -> &'static KernelServices {
    *GLOBAL_SERVICES
        .get()
        .expect("KernelServices global not set")
}
