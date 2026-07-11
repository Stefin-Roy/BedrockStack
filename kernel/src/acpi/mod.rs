use core::ptr::{read_volatile, write_volatile, NonNull};
use alloc::sync::Arc;
use acpi::{
    AcpiError, AcpiTables, Handler, Handle, PciAddress, PhysicalMapping,
};
use acpi::platform::AcpiPlatform;
use acpi::aml::{AmlError, Interpreter as AmlInterpreter};

pub use acpi::platform::pci::PciConfigRegions;
pub use acpi::platform::interrupt::InterruptModel;

/// Identity-mapped ACPI handler for BedrockOS.
///
/// Since the kernel maps all physical memory with identity page tables,
/// `map_physical_region` returns the physical address as the virtual
/// address.  Unmap is a no-op.
#[derive(Clone)]
pub struct AcpiHandler;

impl Handler for AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: NonNull::new(physical_address as *mut T).unwrap(),
            region_length: size,
            mapped_length: size,
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

    fn read_pci_u8(&self, _address: PciAddress, _offset: u16) -> u8 {
        log::warn!("ACPI: PCI config read not implemented");
        0
    }

    fn read_pci_u16(&self, _address: PciAddress, _offset: u16) -> u16 {
        log::warn!("ACPI: PCI config read not implemented");
        0
    }

    fn read_pci_u32(&self, _address: PciAddress, _offset: u16) -> u32 {
        log::warn!("ACPI: PCI config read not implemented");
        0
    }

    fn write_pci_u8(&self, _address: PciAddress, _offset: u16, _value: u8) {
        log::warn!("ACPI: PCI config write not implemented");
    }

    fn write_pci_u16(&self, _address: PciAddress, _offset: u16, _value: u16) {
        log::warn!("ACPI: PCI config write not implemented");
    }

    fn write_pci_u32(&self, _address: PciAddress, _offset: u16, _value: u32) {
        log::warn!("ACPI: PCI config write not implemented");
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
}
