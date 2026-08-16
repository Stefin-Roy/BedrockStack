use alloc::vec::Vec;

use crate::acpi::AcpiSubsystem;
use crate::smp::ApContext;

pub trait CpuManager: Send + Sync {
    fn current_cpu_id(&self) -> u32;
    fn cpu_count(&self) -> u32;
    fn send_ipi(&self, cpu_id: u32, vector: u8);
    fn broadcast_ipi_except_self(&self, vector: u8);
    fn discover_cpus(&self, acpi: Option<&AcpiSubsystem>) -> Vec<(u32, bool)>;
    unsafe fn wake_aps(&self, page_table_root: u64, aps: &[ApContext]) -> usize;
}
