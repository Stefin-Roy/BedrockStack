use core::ptr::{read_volatile, write_volatile, NonNull};
use alloc::sync::Arc;
use acpi::{
    AcpiError, AcpiTables, Handler, Handle, PciAddress, PhysicalMapping,
};
use acpi::platform::AcpiPlatform;
use acpi::registers::Pm1ControlBit;
use acpi::aml::AmlError;
use acpi::aml::Interpreter as AmlInterpreter;
use acpi::address::MappedGas;
use acpi::sdt::fadt::Fadt;
use spin::Mutex;

pub use acpi::platform::pci::PciConfigRegions;
pub use acpi::platform::interrupt::InterruptModel;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{Vmm, PageFlags, KERNEL_VMA_BASE};

// ── VMM state for ACPI mapping ─────────────────────────────────────────

/// Virtual address floor for ACPI-mapped tables.
/// 256 MB below the kernel VMA base → ~0xFFFFFF7F00000000.
const ACPI_VADDR_BASE: u64 = KERNEL_VMA_BASE - 0x10000000;

struct AcpiVmmState {
    root: u64,
    alloc: *mut BitmapAllocator,
    next_vaddr: u64,
}

// Safety: AcpiVmmState is always accessed behind a Mutex; the raw allocator
// pointer is only dereferenced while the lock is held and the kernel is
// single-threaded during init.
unsafe impl Send for AcpiVmmState {}
unsafe impl Sync for AcpiVmmState {}

static ACPI_STATE: Mutex<Option<AcpiVmmState>> = Mutex::new(None);

/// Initialise the ACPI VMM state.
///
/// Must be called once after the higher-half page tables are activated and
/// before any `AcpiSubsystem::new()` call.
pub fn init_vmm(root: u64, alloc: *mut BitmapAllocator) {
    *ACPI_STATE.lock() = Some(AcpiVmmState {
        root,
        alloc,
        next_vaddr: ACPI_VADDR_BASE,
    });
}

/// Map a physical MMIO region through the ACPI VMM.
///
/// Used by IOAPIC and other device drivers to map MMIO regions into the
/// reserved virtual address range.  Returns the virtual address.
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

// ── ACPI handler ───────────────────────────────────────────────────────

/// ACPI handler for BedrockOS.
///
/// `map_physical_region` uses the VMM to create page-table entries inside
/// a reserved virtual address range just below `KERNEL_VMA_BASE`.  Unmap is
/// a no-op (the range is large and never reused).
#[derive(Clone)]
pub struct AcpiHandler;

impl Handler for AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let page_offset = (physical_address & 0xFFF) as u64;
        let aligned_phys = (physical_address & !0xFFF) as u64;
        let aligned_size = ((page_offset + size as u64) + 0xFFF) & !0xFFF;

        let mut state_guard = ACPI_STATE.lock();
        let state = state_guard.as_mut()
            .expect("ACPI VMM state not initialised — call acpi::init_vmm() first");
        let vaddr = state.next_vaddr - aligned_size;
        state.next_vaddr = vaddr;

        let mut vmm = Vmm::from_root(state.root);
        // Safety: we hold the Mutex and interrupts are not enabled yet during
        // init; the raw allocator pointer is valid for the kernel's lifetime.
        let alloc = unsafe { &mut *state.alloc };
        vmm.map(alloc, vaddr, aligned_phys, aligned_size, PageFlags::READ | PageFlags::WRITE);

        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: NonNull::new((vaddr + page_offset) as *mut T).unwrap(),
            region_length: size,
            mapped_length: aligned_size as usize,
            handler: self.clone(),
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { read_volatile(address as *const u8) }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { read_volatile(address as *const u16) }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { read_volatile(address as *const u32) }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { read_volatile(address as *const u64) }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe { write_volatile(address as *mut u8, value) }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe { write_volatile(address as *mut u16, value) }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe { write_volatile(address as *mut u32, value) }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe { write_volatile(address as *mut u64, value) }
    }

    #[cfg(target_arch = "x86_64")]
    fn read_io_u8(&self, port: u16) -> u8 {
        let mut val: u8;
        unsafe {
            core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
        }
        val
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn read_io_u8(&self, _port: u16) -> u8 { 0 }

    #[cfg(target_arch = "x86_64")]
    fn read_io_u16(&self, port: u16) -> u16 {
        let mut val: u16;
        unsafe {
            core::arch::asm!("in ax, dx", in("dx") port, out("ax") val, options(nomem, nostack, preserves_flags));
        }
        val
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn read_io_u16(&self, _port: u16) -> u16 { 0 }

    #[cfg(target_arch = "x86_64")]
    fn read_io_u32(&self, port: u16) -> u32 {
        let mut val: u32;
        unsafe {
            core::arch::asm!("in eax, dx", in("dx") port, out("eax") val, options(nomem, nostack, preserves_flags));
        }
        val
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn read_io_u32(&self, _port: u16) -> u32 { 0 }

    #[cfg(target_arch = "x86_64")]
    fn write_io_u8(&self, port: u16, value: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn write_io_u8(&self, _port: u16, _value: u8) {}

    #[cfg(target_arch = "x86_64")]
    fn write_io_u16(&self, port: u16, value: u16) {
        unsafe {
            core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn write_io_u16(&self, _port: u16, _value: u16) {}

    #[cfg(target_arch = "x86_64")]
    fn write_io_u32(&self, port: u16, value: u32) {
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn write_io_u32(&self, _port: u16, _value: u32) {}

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        crate::pci::ecam::read_u8(address.segment(), address.bus(), address.device(), address.function(), offset)
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        crate::pci::ecam::read_u16(address.segment(), address.bus(), address.device(), address.function(), offset)
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        crate::pci::ecam::read_u32(address.segment(), address.bus(), address.device(), address.function(), offset)
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, value: u8) {
        crate::pci::ecam::write_u8(address.segment(), address.bus(), address.device(), address.function(), offset, value);
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, value: u16) {
        crate::pci::ecam::write_u16(address.segment(), address.bus(), address.device(), address.function(), offset, value);
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, value: u32) {
        crate::pci::ecam::write_u32(address.segment(), address.bus(), address.device(), address.function(), offset, value);
    }

    fn nanos_since_boot(&self) -> u64 {
        0
    }

    fn stall(&self, microseconds: u64) {
        for _ in 0..microseconds.wrapping_mul(100) {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        self.stall(milliseconds.wrapping_mul(1000));
    }

    fn create_mutex(&self) -> Handle {
        Handle(0)
    }

    fn acquire(&self, _mutex: Handle, _timeout: u16) -> Result<(), AmlError> {
        Ok(())
    }

    fn release(&self, _mutex: Handle) {}
}

/// ACPI subsystem state, parsed from the RSDP on boot.
pub struct AcpiSubsystem {
    pub platform: Arc<AcpiPlatform<AcpiHandler>>,
    pub pci_config_regions: PciConfigRegions,
    pub aml: Option<AmlInterpreter<AcpiHandler>>,
}

impl AcpiSubsystem {
    /// Parse all ACPI tables starting from the RSDP at `rsdp_addr`.
    pub fn new(rsdp_addr: u64) -> Result<Self, AcpiError> {
        log::info!("ACPI: RSDP at 0x{:x}", rsdp_addr);
        let handler = AcpiHandler;
        let tables = unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp_addr as usize)? };
        let platform = Arc::new(AcpiPlatform::new(tables, handler)?);
        log::info!("ACPI: platform info parsed (interrupt model: {:?})",
            platform.interrupt_model);

        let pci_config_regions = PciConfigRegions::new(&platform.tables)?;
        log::info!("ACPI: {} PCI config regions", pci_config_regions.regions.len());

        Ok(Self { platform, pci_config_regions, aml: None })
    }

    /// Initialise the AML interpreter from DSDT / SSDTs.
    pub fn init_aml(&mut self) -> Result<(), AcpiError> {
        let aml = AmlInterpreter::new_from_platform(&self.platform)?;
        log::info!("ACPI: AML interpreter initialized");
        self.aml = Some(aml);
        Ok(())
    }

    // ── Reset ──────────────────────────────────────────────────────────

    /// Attempt a system reset via the FADT reset register.
    ///
    /// Falls back to the legacy 8042 PS/2 controller method on x86, then
    /// enters an infinite halt loop if neither works.
    pub fn reset(&self) -> ! {
        log::info!("ACPI: system reset requested");

        // 1. FADT reset register method
        if let Some(fadt) = self.platform.tables.find_table::<Fadt>() {
            // Copy fields from the packed struct to avoid unaligned references.
            let reset_val = fadt.reset_value;
            let flags = fadt.flags;
            if flags.supports_system_reset_via_fadt() {
                log::info!("ACPI: reset via FADT reset register");
                if let Ok(reset_gas) = fadt.reset_register() {
                    let handler = AcpiHandler;
                    if let Ok(mapped) = unsafe { MappedGas::map_gas(reset_gas, &handler) } {
                        let _ = mapped.write(reset_val as u64);
                        // If reset succeeded the CPU should be gone by now.
                    }
                }
            }
        }

        // 2. Legacy 8042 keyboard controller reset (x86 only)
        #[cfg(target_arch = "x86_64")]
        {
            log::info!("ACPI: reset via 8042 keyboard controller");
            let handler = AcpiHandler;
            // Wait for the keyboard controller to be ready (status bit 1 must be 0).
            for _ in 0..100_000 {
                if handler.read_io_u8(0x64) & 0x02 == 0 {
                    break;
                }
            }
            handler.write_io_u8(0x64, 0xFE);
        }

        // 3. RISC-V: try SBI SRST cold reboot (diverging).
        #[cfg(target_arch = "riscv64")]
        crate::arch::riscv64::sbi::cold_reboot();

        // 4. Last resort: halt forever.
        #[cfg(target_arch = "x86_64")]
        {
            log::error!("ACPI: reset failed — halting");
            loop {
                unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
            }
        }
    }

    // ── Shutdown ───────────────────────────────────────────────────────

    /// Attempt a graceful system shutdown (S5 soft-off) via the ACPI PM1
    /// control registers.
    ///
    /// The SLP_TYP value for S5 is read from the AML namespace (`\_S5`) if
    /// the interpreter is available, otherwise a default value of 0 is used
    /// (works on most QEMU / Bochs / common virtual hardware).
    ///
    /// On x86 a legacy PM IO-port write is also tried as a fallback.
    pub fn shutdown(&self) -> ! {
        log::info!("ACPI: system shutdown requested");

        // Determine the SLP_TYP value for S5.
        // In the ACPI specification the \_S5 object contains a package
        //   Package { 0x05, 0x00, 0x00, 0x00 }
        // where the second element is the SLP_TYPa value for S5.
        let slp_typ_s5: u8 = self.s5_slp_typ().unwrap_or(0x00);
        log::info!("ACPI: S5 SLP_TYP = 0x{:02x}", slp_typ_s5);

        // Write SLP_TYP and then assert SLP_EN in the PM1 control register.
        let ctrl = &self.platform.registers.pm1_control_registers;
        if ctrl.set_sleep_typ(slp_typ_s5).is_ok() {
            if ctrl.set_bit(Pm1ControlBit::SleepEnable, true).is_ok() {
                // If shutdown succeeded we should never reach here.
            }
        }

        // Fallback: QEMU / legacy PM IO port on x86.
        #[cfg(target_arch = "x86_64")]
        {
            log::info!("ACPI: shutdown fallback — QEMU PM IO port");
            // PM1a_CNT (IO port) — write SLP_TYP=0, SLP_EN=1
            let pm1a_port = {
                let fadt_opt = self.platform.tables.find_table::<Fadt>();
                if let Some(fadt_ref) = fadt_opt {
                    // Copy to avoid unaligned access on the packed struct.
                    let fadt = *fadt_ref;
                    fadt.pm1a_control_block().ok().map(|gas| gas.address as u16)
                } else {
                    None
                }
            }.unwrap_or(0x600); // QEMU ICH9 default
            let val: u16 = (0x00u16 << 10) | (1u16 << 13); // SLP_TYP=0 + SLP_EN
            let handler = AcpiHandler;
            handler.write_io_u16(pm1a_port, val);
        }

        // RISC-V: try SBI SRST shutdown (diverging).
        #[cfg(target_arch = "riscv64")]
        crate::arch::riscv64::sbi::system_reset();

        // Last resort: halt forever.
        #[cfg(target_arch = "x86_64")]
        {
            log::error!("ACPI: shutdown failed — halting");
            loop {
                unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
            }
        }
    }

    /// Try to read the SLP_TYP value for S5 from the AML namespace.
    fn s5_slp_typ(&self) -> Option<u8> {
        use acpi::aml::namespace::AmlName;
        use core::str::FromStr;
        let aml = self.aml.as_ref()?;
        let path = AmlName::from_str("\\_S5").ok()?;
        let result = aml.evaluate(path, alloc::vec![]).ok()?;
        // \_S5 is a Package of { Integer, Integer, Integer, Integer }.
        // The second element (index 1) is the SLP_TYPa value for S5.
        if let acpi::aml::object::Object::Package(elements) = &*result {
            if elements.len() >= 2 {
                if let Ok(val) = elements[1].as_integer() {
                    return Some(val as u8);
                }
            }
        }
        None
    }
}
