use crate::mm::vmm::PageFlags;

pub use crate::acpi::{
    DeviceScope, DmarInfo, Drhd, InterruptModel, IoApic, PciConfigRegions, PlatformInfo, Rmrr,
};

pub trait AcpiProvider: Send + Sync {
    fn interrupt_model(&self) -> &InterruptModel;
    fn pci_config_regions(&self) -> &PciConfigRegions;
    fn platform_info(&self) -> Option<&PlatformInfo>;
    fn cpus(&self) -> &[(u32, bool)];
    fn dmar(&self) -> Option<&DmarInfo>;

    /// Map a physical MMIO region and return its virtual address.
    fn map_device_mmio(&self, paddr: u64, size: u64, flags: PageFlags) -> u64;

    /// Invoke an AML control method on the persistent interpreter (x86_64
    /// only; RISC-V has no ACPI tables and no implementation of this item).
    #[cfg(target_arch = "x86_64")]
    fn aml_invoke(
        &self,
        path: &str,
        args: ::aml::value::Args,
    ) -> Result<::aml::AmlValue, ::aml::AmlError>;
}
