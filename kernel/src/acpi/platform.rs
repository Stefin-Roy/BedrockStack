use alloc::vec::Vec;

/// Generic Address Structure (ACPI 2.0+).
#[derive(Clone, Debug)]
pub struct Gas {
    pub address_space_id: u8, // 0=system memory, 1=system I/O
    pub register_bit_width: u8,
    pub register_bit_offset: u8,
    pub access_size: u8,
    pub address: u64,
}

#[derive(Clone, Debug)]
pub struct PciMcfgRegion {
    pub pci_segment_group: u16,
    pub bus_number_start: u8,
    pub bus_number_end: u8,
    pub base_address: u64,
}

#[derive(Clone, Debug)]
pub struct PciConfigRegions {
    pub regions: Vec<PciMcfgRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpiError {
    BadSignature,
    BadChecksum,
    TableNotFound,
    InvalidData,
    Unsupported,
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct Processor {
    pub local_apic_id: u32,
    pub state: ProcessorState,
    pub is_ap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcessorState {
    Disabled,
    Enabled,
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct ProcessorInfo {
    pub boot_processor: Processor,
    pub application_processors: Vec<Processor>,
}

#[derive(Clone, Debug)]
pub struct IoApic {
    pub address: u64,
    pub global_system_interrupt_base: u32,
}

#[derive(Clone, Debug)]
pub struct Apic {
    pub io_apics: Vec<IoApic>,
    pub local_apic_address: u64,
}

#[derive(Clone, Debug)]
pub enum InterruptModel {
    Apic(Apic),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pm1ControlBit {
    SleepEnable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    ActiveHigh,
    ActiveLow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerMode {
    Edge,
    Level,
}

/// VT-d DMA Remapping Reporting Structure (DMAR) — Intel VT-d, ACPI View.
///
/// Probed from the `DMAR` table. `None` when absent (IOMMU unavailable
/// or firmware did not expose it, e.g. legacy QEMU without `intel-iommu`).
#[derive(Clone, Debug)]
pub struct DeviceScope {
    pub device_type: u8,
    pub enumeration_id: u8,
    pub start_bus_number: u8,
    /// PCI path: list of (device, function) hops from the remapping unit.
    pub path: Vec<(u8, u8)>,
}

#[derive(Clone, Debug)]
pub struct Drhd {
    pub flags: u8,
    pub segment: u16,
    pub register_base: u64,
    pub include_pci_all: bool,
    pub devices: Vec<DeviceScope>,
}

#[derive(Clone, Debug)]
pub struct Rmrr {
    pub segment: u16,
    pub base_address: u64,
    pub limit_address: u64,
    pub devices: Vec<DeviceScope>,
}

#[derive(Clone, Debug)]
pub struct Atsr {
    pub flags: u8,
    pub segment: u16,
    pub devices: Vec<DeviceScope>,
}

#[derive(Clone, Debug)]
pub struct DmarInfo {
    pub host_address_width: u8,
    pub flags: u8,
    pub drhds: Vec<Drhd>,
    pub rmrrs: Vec<Rmrr>,
    pub atsr: Vec<Atsr>,
}

/// Platform-level ACPI information parsed from FADT.
#[derive(Clone, Debug)]
pub struct PlatformInfo {
    pub reset_gas: Option<Gas>,
    pub reset_value: u8,
    pub reset_supported: bool,
    pub pm1_control: crate::acpi::fadt::Pm1ControlRegisters,
    /// SLP_TYP value for the S5 soft-off state, decoded from the `\_S5`
    /// AML package.  `None` means the value is not known and the PM1
    /// registers must not be programmed with a guessed value.
    pub slp_typ_s5: Option<u8>,
}
