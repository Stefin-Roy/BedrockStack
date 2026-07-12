use spin::Mutex;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{Vmm, PageFlags, KERNEL_VMA_BASE};

mod tables;
mod mcfg;
mod fadt;
mod gas;
mod madt;
pub mod platform;

pub use platform::{
    AcpiError, Apic, Gas, InterruptModel, IoApic, PciConfigRegions, PciMcfgRegion,
    Pm1ControlBit, Polarity, PlatformInfo, Processor, ProcessorInfo, ProcessorState, TriggerMode,
};

/// ACPI VMM state for mapping physical regions.
const ACPI_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x10000000;

struct AcpiVmmState {
    root: u64,
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
}

unsafe impl Send for AcpiVmmState {}
unsafe impl Sync for AcpiVmmState {}

static ACPI_STATE: Mutex<Option<AcpiVmmState>> = Mutex::new(None);

/// Initialise the ACPI VMM state. Must be called once after higher-half page
/// tables are activated and before any `AcpiSubsystem::new()` call.
pub fn init_vmm(root: u64, alloc: *mut BitmapAllocator) {
    *ACPI_STATE.lock() = Some(AcpiVmmState { root, alloc, next_vaddr: ACPI_VADDR_BASE });
}

/// Map a physical MMIO region through the ACPI VMM.
pub fn map_device_mmio(paddr: u64, size: u64, flags: PageFlags) -> u64 {
    let mut guard = ACPI_STATE.lock();
    let state = guard.as_mut().expect("ACPI VMM not initialized — call init_vmm first");
    let vaddr = state.next_vaddr - size;
    state.next_vaddr = vaddr;
    let mut vmm = Vmm::from_root(state.root);
    let alloc = unsafe { &mut *state.alloc };
    vmm.map(alloc, vaddr, paddr, size, flags);
    vaddr
}

fn sig(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// ACPI subsystem state, parsed from the RSDP on boot.
pub struct AcpiSubsystem {
    pub interrupt_model: InterruptModel,
    pub processor_info: Option<ProcessorInfo>,
    pub pci_config_regions: PciConfigRegions,
    pub platform_info: PlatformInfo,
}

impl AcpiSubsystem {
    /// Parse all ACPI tables starting from the RSDP at `rsdp_addr`.
    pub fn new(rsdp_addr: u64) -> Result<Self, AcpiError> {
        log::info!("ACPI: RSDP at 0x{:x}", rsdp_addr);
        let entries = tables::parse_tables(rsdp_addr)?;

        let fadt_fields = entries
            .iter()
            .find(|e| e.signature == sig(b"FACP"))
            .map(|e| fadt::parse_fadt(e.vaddr, e.length))
            .unwrap_or(Err(AcpiError::TableNotFound))?;

        let pci_config_regions = entries
            .iter()
            .find(|e| e.signature == sig(b"MCFG"))
            .and_then(|e| mcfg::parse_mcfg(e.vaddr, e.length).ok())
            .unwrap_or(PciConfigRegions { regions: alloc::vec::Vec::new() });
        log::info!("ACPI: {} PCI config regions", pci_config_regions.regions.len());

        let (interrupt_model, processor_info) = entries
            .iter()
            .find(|e| e.signature == sig(b"APIC"))
            .and_then(|e| madt::parse_madt(e.vaddr, e.length).ok())
            .unwrap_or((InterruptModel::Unknown, None));

        let platform_info = PlatformInfo {
            reset_gas: fadt_fields.reset_gas,
            reset_value: fadt_fields.reset_value,
            reset_supported: fadt_fields.reset_supported,
            pm1_control: fadt_fields.pm1_control,
        };

        log::info!("ACPI: platform info parsed (interrupt model: {:?})", interrupt_model);

        Ok(Self { interrupt_model, processor_info, pci_config_regions, platform_info })
    }

    /// Attempt a system reset via the FADT reset register, with fallbacks.
    pub fn reset(&self) -> ! {
        log::info!("ACPI: system reset requested");

        if self.platform_info.reset_supported {
            if let Some(ref reset_gas) = self.platform_info.reset_gas {
                log::info!("ACPI: reset via FADT reset register");
                gas::gas_write(reset_gas, self.platform_info.reset_value as u64);
            }
        }

        #[cfg(target_arch = "x86_64")]
        {
            log::info!("ACPI: reset via 8042 keyboard controller");
            for _ in 0..100_000 {
                let mut status: u8;
                unsafe { core::arch::asm!("in al, dx", in("dx") 0x64u16, out("al") status, options(nomem, nostack, preserves_flags)); }
                if status & 0x02 == 0 { break; }
            }
            unsafe { core::arch::asm!("out dx, al", in("dx") 0x64u16, in("al") 0xFEu8, options(nomem, nostack, preserves_flags)); }
        }

        #[cfg(target_arch = "riscv64")]
        crate::arch::riscv64::sbi::cold_reboot();

        #[cfg(target_arch = "x86_64")]
        {
            log::error!("ACPI: reset failed — halting");
            loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)) } }
        }
    }

    /// Attempt a graceful system shutdown (S5 soft-off) via the PM1 control
    /// registers. Falls back to QEMU legacy port on x86.
    pub fn shutdown(&self) -> ! {
        log::info!("ACPI: system shutdown requested");

        // SLP_TYP for S5 — AML is not available, use default 0x00
        // (works on QEMU / Bochs / common virtual hardware).
        let slp_typ_s5: u8 = 0x00;
        log::info!("ACPI: S5 SLP_TYP = 0x{:02x}", slp_typ_s5);

        let ctrl = &self.platform_info.pm1_control;
        let _ = ctrl.set_sleep_typ(slp_typ_s5);
        let _ = ctrl.set_bit(Pm1ControlBit::SleepEnable, true);

        #[cfg(target_arch = "x86_64")]
        {
            log::info!("ACPI: shutdown fallback — QEMU PM IO port");
            let pm1a_port = self.platform_info.pm1_control.pm1a.address as u16;
            let val: u16 = (0x00u16 << 10) | (1u16 << 13);
            unsafe { core::arch::asm!("out dx, ax", in("dx") pm1a_port, in("ax") val, options(nomem, nostack, preserves_flags)); }
        }

        #[cfg(target_arch = "riscv64")]
        crate::arch::riscv64::sbi::system_reset();

        #[cfg(target_arch = "x86_64")]
        {
            log::error!("ACPI: shutdown failed — halting");
            loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)) } }
        }
    }
}
