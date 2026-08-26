use alloc::vec::Vec;
use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{KERNEL_VMA_BASE, PageFlags, Vmm};

#[cfg(target_arch = "x86_64")]
mod aml_ctx;
mod dmar;
mod fadt;
mod gas;
#[cfg(target_arch = "x86_64")]
mod handler;
mod madt;
mod mcfg;
pub mod platform;
mod tables;

pub use platform::{
    AcpiError, Apic, Atsr, DeviceScope, DmarInfo, Drhd, Gas, InterruptModel, IoApic,
    PciConfigRegions, PciMcfgRegion, PlatformInfo, Pm1ControlBit, Polarity, Processor,
    ProcessorInfo, ProcessorState, Rmrr, TriggerMode,
};

use alloc::sync::Arc;
use spin::Once;

static ACPI_GLOBAL: Once<Arc<AcpiSubsystem>> = Once::new();

pub fn set_global_snapshot(sub: AcpiSubsystem) {
    if ACPI_GLOBAL.get().is_some() {
        crate::drivers::serial::SerialPort::puts("[acpi] WARN: global snapshot already set, ignoring duplicate\n");
        return;
    }
    // Use Once directly so racy duplicate does not allocate an extra Arc.
    ACPI_GLOBAL.call_once(|| Arc::new(sub));
    // If two callers raced past the early check, the loser’s closure is
    // dropped without warning — still at-most-once, just no duplicate log.
}

pub fn global_snapshot() -> Option<Arc<AcpiSubsystem>> {
    ACPI_GLOBAL.get().cloned()
}

pub fn global_cpus() -> alloc::vec::Vec<(u32, bool)> {
    global_snapshot().map(|s| s.cpus.clone()).unwrap_or_default()
}

/// Resolve a legacy ISA IRQ to its GSI plus polarity/trigger via the MADT
/// interrupt source override table.  Returns `None` when no override exists
/// (the caller should then assume the default ISA wiring).
pub fn irq_override(irq: u8) -> Option<(u32, Polarity, TriggerMode)> {
    madt::irq_override(irq)
}

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
    *ACPI_STATE.lock() = Some(AcpiVmmState {
        root,
        alloc,
        next_vaddr: ACPI_VADDR_BASE,
    });
}

/// Update the allocator pointer after the BitmapAllocator has been moved
/// (e.g. into Kernel). The ACPI VMM stashes a raw pointer, so it must be
/// rebased when the allocator moves.
pub fn update_alloc(alloc: *mut BitmapAllocator) {
    if let Some(state) = ACPI_STATE.lock().as_mut() {
        state.alloc = alloc;
    }
}

/// ACPI VMM floor — 512 MB of virtual space for ACPI tables (generous).
const ACPI_VADDR_FLOOR: u64 = ACPI_VADDR_BASE - 0x2000_0000;

/// Map a physical MMIO region through the ACPI VMM.
/// Returns `Err` on VMM exhaustion instead of panicking — a malformed ACPI
/// table could otherwise force a `panic=abort`.
pub fn map_device_mmio(paddr: u64, size: u64, flags: PageFlags) -> u64 {
    try_map_device_mmio(paddr, size, flags).unwrap_or_else(|e| {
        log::error!("ACPI VMM exhaustion: {} (paddr={:#x} size={:#x})", e, paddr, size);
        // Exhaustion is fatal for this boot path; loop rather than `panic=abort`.
        // Callers that can handle failure should use `try_map_device_mmio`.
        loop {
            core::hint::spin_loop();
        }
    })
}

/// Fallible variant — returns `Err` on VMM exhaustion or uninitialized state.
pub fn try_map_device_mmio(paddr: u64, size: u64, flags: PageFlags) -> Result<u64, &'static str> {
    let mut guard = ACPI_STATE.lock();
    let state = guard
        .as_mut()
        .ok_or("ACPI VMM not initialized — call init_vmm first")?;
    // Page-round the reservation so successive small mappings can never
    // overlap within a shared page.
    let pages = (size + 0xFFF) & !0xFFF;
    let vaddr = state
        .next_vaddr
        .checked_sub(pages)
        .ok_or("ACPI VMM: address space exhausted (overflow)")?;
    if vaddr < ACPI_VADDR_FLOOR {
        return Err("ACPI VMM: address space exhausted (would overlap adjacent region)");
    }
    state.next_vaddr = vaddr;
    let mut vmm = Vmm::from_root(state.root);
    let alloc = unsafe { &mut *state.alloc };
    vmm.map(alloc, vaddr, paddr, pages, flags);
    Ok(vaddr)
}

fn sig(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// ACPI subsystem state, parsed from the RSDP on boot.
pub struct AcpiSubsystem {
    pub interrupt_model: InterruptModel,
    pub processor_info: Option<ProcessorInfo>,
    pub cpus: Vec<(u32, bool)>,
    pub pci_config_regions: PciConfigRegions,
    pub platform_info: PlatformInfo,
    /// DMA remapping (VT-d) info, if present.
    pub dmar: Option<DmarInfo>,
    /// Number of SDT entries successfully parsed (for introspection).
    pub table_count: usize,
    /// Persistent AML interpreter over the DSDT + SSDTs (x86_64). `None` when
    /// no DSDT was found or the tables could not be parsed by the `aml` crate.
    #[cfg(target_arch = "x86_64")]
    pub aml: Option<spin::Mutex<::aml::AmlContext>>,
}

impl AcpiSubsystem {
    /// Parse all ACPI tables starting from the RSDP at `rsdp_addr`.
    ///
    /// When `rsdp_data` is `Some(...)` the RSDP data is already available in
    /// memory (e.g. embedded in a Multiboot2 tag) and is used directly;
    /// otherwise the RSDP is mapped from physical `rsdp_addr`.
    pub fn new(rsdp_addr: u64, rsdp_data: Option<&'static [u8]>) -> Result<Self, AcpiError> {
        log::info!("ACPI: RSDP at 0x{:x}", rsdp_addr);
        let entries = if let Some(data) = rsdp_data {
            tables::parse_tables_from_data(data)?
        } else {
            tables::parse_tables(rsdp_addr)?
        };

        let fadt_fields = entries
            .iter()
            .find(|e| e.signature == sig(b"FACP"))
            .map(|e| fadt::parse_fadt(e.vaddr, e.length))
            .unwrap_or(Err(AcpiError::TableNotFound))?;

        let pci_config_regions = entries
            .iter()
            .find(|e| e.signature == sig(b"MCFG"))
            .and_then(|e| mcfg::parse_mcfg(e.vaddr, e.length).ok())
            .unwrap_or(PciConfigRegions {
                regions: alloc::vec::Vec::new(),
            });
        log::info!(
            "ACPI: {} PCI config regions",
            pci_config_regions.regions.len()
        );

        let (interrupt_model, processor_info) = entries
            .iter()
            .find(|e| e.signature == sig(b"APIC"))
            .and_then(|e| madt::parse_madt(e.vaddr, e.phys_addr, e.length).ok())
            .unwrap_or((InterruptModel::Unknown, None));

        if let Some(ref pi) = processor_info {
            SerialPort::puts("[acpi] after parse_madt: boot=");
            SerialPort::put_u64(pi.boot_processor.local_apic_id as u64);
            SerialPort::puts(" aps=");
            for p in &pi.application_processors {
                SerialPort::put_u64(p.local_apic_id as u64);
                SerialPort::puts(" ");
            }
            SerialPort::puts("\n");
        } else {
            SerialPort::puts("[acpi] after parse_madt: processor_info is None\n");
        }

        // Build a direct CPU list that bypasses the ProcessorInfo struct
        // to avoid potential layout/corruption issues when reading later.
        let mut cpus: Vec<(u32, bool)> = Vec::new();
        if let Some(ref pi) = processor_info {
            cpus.push((pi.boot_processor.local_apic_id, true));
            for p in &pi.application_processors {
                let enabled = p.state != ProcessorState::Disabled;
                cpus.push((p.local_apic_id, enabled));
            }
        }

        #[cfg(target_arch = "x86_64")]
        let (aml, slp_typ_s5) = Self::aml_boot(&entries, fadt_fields.dsdt_addr as u64);
        #[cfg(not(target_arch = "x86_64"))]
        let slp_typ_s5 = None;

        let platform_info = PlatformInfo {
            reset_gas: fadt_fields.reset_gas,
            reset_value: fadt_fields.reset_value,
            reset_supported: fadt_fields.reset_supported,
            pm1_control: fadt_fields.pm1_control,
            slp_typ_s5,
        };

        log::info!(
            "ACPI: platform info parsed (interrupt model: {:?})",
            interrupt_model
        );

        let dmar = entries
            .iter()
            .find(|e| e.signature == sig(b"DMAR"))
            .and_then(|e| match dmar::parse_dmar(e.vaddr, e.length) {
                Ok(v) => Some(v),
                Err(err) => {
                    log::warn!("ACPI: DMAR parse failed: {:?} (ignored)", err);
                    None
                }
            });
        if let Some(ref dm) = dmar {
            SerialPort::puts("[acpi] DMAR host_width=");
            SerialPort::put_u64(dm.host_address_width as u64);
            SerialPort::puts(" drhd=");
            SerialPort::put_u64(dm.drhds.len() as u64);
            SerialPort::puts(" rmrr=");
            SerialPort::put_u64(dm.rmrrs.len() as u64);
            SerialPort::puts("\n");
        } else {
            SerialPort::puts("[acpi] DMAR absent\n");
        }

        let table_count = entries.len();
        #[cfg(target_arch = "x86_64")]
        let subsystem = Self {
            interrupt_model,
            processor_info,
            cpus,
            pci_config_regions,
            platform_info,
            dmar,
            table_count,
            aml,
        };
        #[cfg(not(target_arch = "x86_64"))]
        let subsystem = Self {
            interrupt_model,
            processor_info,
            cpus,
            pci_config_regions,
            platform_info,
            dmar,
            table_count,
        };
        Ok(subsystem)
    }

    /// Initialise the AML interpreter over the DSDT + SSDTs and decode `\_S5`.
    ///
    /// The interpreter is always used. A DSDT parse failure disables the ACPI
    /// PM1 shutdown path loudly rather than guessing. The mainline `aml` crate
    /// is taken unmodified; when it fails on a table the interpreter is
    /// dropped and `\_S5` falls back to `None`.
    #[cfg(target_arch = "x86_64")]
    fn aml_boot(
        entries: &[tables::SdtEntry],
        dsdt_fallback: u64,
    ) -> (Option<spin::Mutex<::aml::AmlContext>>, Option<u8>) {
        if dsdt_fallback == 0 && !entries.iter().any(|e| e.signature == sig(b"DSDT")) {
            log::warn!("ACPI: no DSDT -- ACPI PM1 shutdown disabled");
            return (None, None);
        }

        match aml_ctx::init_aml_ctx(entries, dsdt_fallback) {
            Ok(mut ctx) => {
                match ctx.initialize_objects() {
                    Ok(()) => log::info!("ACPI: AML _INI sweep complete"),
                    Err(e) => log::error!("ACPI: AML _INI sweep failed: {:?}", e),
                }

                let slp = aml_ctx::s5_slp_typa(&mut ctx);
                match slp {
                    Some(t) => log::info!("ACPI: \\_S5 SLP_TYP = 0x{:02x}", t),
                    None => log::warn!("ACPI: \\_S5 not decodable -- ACPI PM1 shutdown disabled"),
                }
                (Some(spin::Mutex::new(ctx)), slp)
            }
            Err(e) => {
                log::error!(
                    "ACPI: AML interpreter init failed: {:?} -- ACPI PM1 shutdown disabled",
                    e
                );
                (None, None)
            }
        }
    }

    /// Invoke an AML control method on the persistent interpreter. Returns
    /// `Err` when no interpreter is available (parsing failed, or RISC-V) or
    /// the method fails.
    #[cfg(target_arch = "x86_64")]
    pub fn aml_invoke(
        &self,
        path: &str,
        args: ::aml::value::Args,
    ) -> Result<::aml::AmlValue, ::aml::AmlError> {
        let ctx = self.aml.as_ref().ok_or(::aml::AmlError::Unimplemented)?;
        let name = ::aml::AmlName::from_str(path)?;
        let mut guard = ctx.lock();
        guard.invoke_method(&name, args)
    }

    /// Attempt a system reset via the FADT reset register, with fallbacks.
    pub fn reset(&self) -> ! {
        log::info!("ACPI: system reset requested");

        if self.platform_info.reset_supported {
            if let Some(ref reset_gas) = self.platform_info.reset_gas {
                log::info!("ACPI: reset via FADT reset register");
                if let Err(e) = gas::gas_write(reset_gas, self.platform_info.reset_value as u64) {
                    log::error!("ACPI: FADT reset register write failed: {:?}", e);
                }
            }
        }

        #[cfg(target_arch = "x86_64")]
        {
            log::info!("ACPI: reset via 8042 keyboard controller");
            for _ in 0..100_000 {
                let mut status: u8;
                unsafe {
                    core::arch::asm!("in al, dx", in("dx") 0x64u16, out("al") status, options(nomem, nostack, preserves_flags));
                }
                if status & 0x02 == 0 {
                    break;
                }
            }
            unsafe {
                core::arch::asm!("out dx, al", in("dx") 0x64u16, in("al") 0xFEu8, options(nomem, nostack, preserves_flags));
            }
        }

        #[cfg(target_arch = "riscv64")]
        crate::arch::riscv64::sbi::cold_reboot();

        #[cfg(target_arch = "x86_64")]
        {
            log::error!("ACPI: reset failed -- halting");
            loop {
                unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
            }
        }
    }

    /// Attempt a graceful system shutdown (S5 soft-off) via the PM1 control
    /// registers.  The SLP_TYP value comes from the `\_S5` AML package; when
    /// it is not known the PM1 registers are left alone (writing a guessed
    /// sleep type can hang real hardware).  Falls back to the QEMU legacy
    /// port on x86.
    pub fn shutdown(&self) -> ! {
        log::info!("ACPI: system shutdown requested");

        #[cfg(target_arch = "x86_64")]
        if let Some(ref ctx) = self.aml {
            if let Err(e) = aml_ctx::prepare_to_sleep(&mut ctx.lock(), 5) {
                log::warn!("ACPI: \\_PTS(5) failed: {:?}", e);
            }
        }

        match self.platform_info.slp_typ_s5 {
            Some(slp_typ_s5) => {
                log::info!("ACPI: S5 SLP_TYP = 0x{:02x} (from \\_S5)", slp_typ_s5);
                let ctrl = &self.platform_info.pm1_control;
                if let Err(e) = ctrl.set_sleep_typ(slp_typ_s5) {
                    log::error!("ACPI: PM1 SLP_TYP write failed: {:?}", e);
                }
                if let Err(e) = ctrl.set_bit(Pm1ControlBit::SleepEnable, true) {
                    log::error!("ACPI: PM1 SLP_EN write failed: {:?}", e);
                }
            }
            None => {
                log::error!("ACPI: \\_S5 unknown -- refusing to write a guessed SLP_TYP");
            }
        }

        #[cfg(target_arch = "x86_64")]
        {
            log::info!("ACPI: shutdown fallback -- QEMU PM IO port");
            let pm1a_port = self.platform_info.pm1_control.pm1a.address as u16;
            let val: u16 = (0x00u16 << 10) | (1u16 << 13);
            unsafe {
                core::arch::asm!("out dx, ax", in("dx") pm1a_port, in("ax") val, options(nomem, nostack, preserves_flags));
            }
        }

        #[cfg(target_arch = "riscv64")]
        crate::arch::riscv64::sbi::system_reset();

        #[cfg(target_arch = "x86_64")]
        {
            log::error!("ACPI: shutdown failed -- halting");
            loop {
                unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
            }
        }
    }
}
