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
pub struct Processor {
    pub id: u32,
    pub state: ProcessorState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessorState {
    Disabled,
    Enabled,
}

#[derive(Clone, Debug)]
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
