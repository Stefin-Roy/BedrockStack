use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::filesystems::vfs::irq::IrqMutex;

use super::traits::BlockDevice;
use crate::pci::PciDevice;
use crate::services::dma::DmaAllocator;

pub trait StorageDriver: Send + Sync {
    fn name(&self) -> &str;
    fn probe(&self, dev: &PciDevice) -> bool;
    fn init_controller(
        &self,
        dev: &PciDevice,
        dma: &dyn DmaAllocator,
    ) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str>;
}

static REGISTRY: IrqMutex<Vec<&'static dyn StorageDriver>> = IrqMutex::new(Vec::new());

pub static BLOCK_DEVICES: IrqMutex<Vec<Arc<dyn BlockDevice>>> = IrqMutex::new(Vec::new());

/// Append devices that are not already registered.  Deduplicate by
/// Arc pointer equality only: two distinct physical disks may share the
/// same model and capacity (identical USB sticks), so (model, sectors)
/// is not a unique key.  Replug creates a fresh Arc; the stale entry
/// remains until explicitly pruned, but lib.rs now tries every block
/// device for the ESP mount so a stale first entry does not hide its
/// replacement.
pub fn register_block_devices(new_devices: Vec<Arc<dyn BlockDevice>>) {
    let mut list = BLOCK_DEVICES.lock();
    for d in new_devices {
        if list.iter().any(|e| Arc::ptr_eq(e, &d)) {
            continue;
        }
        list.push(d);
    }
}

pub fn register(driver: &'static dyn StorageDriver) {
    REGISTRY.lock().push(driver);
}

fn register_all() {
    #[cfg(target_arch = "x86_64")]
    register(&super::ahci::AhciDriver);
}

pub fn init_all(pci_devices: &[PciDevice]) -> Vec<Arc<dyn BlockDevice>> {
    use crate::drivers::serial::SerialPort;

    register_all();

    let dma: &dyn DmaAllocator = crate::services::kernel_services().dma;
    let mut all_devices = Vec::new();
    let registry = REGISTRY.lock();

    for dev in pci_devices {
        for driver in registry.iter() {
            if driver.probe(dev) {
                SerialPort::puts("[storage] ");
                SerialPort::puts(driver.name());
                SerialPort::puts(" probe: ");
                SerialPort::put_u64(dev.bus as u64);
                SerialPort::puts(":");
                SerialPort::put_u64(dev.device as u64);
                SerialPort::puts(":");
                SerialPort::put_u64(dev.function as u64);
                SerialPort::puts("\n");
                match driver.init_controller(dev, dma) {
                    Ok(devices) => {
                        let n = devices.len();
                        SerialPort::puts("[storage] ");
                        SerialPort::put_u64(n as u64);
                        SerialPort::puts(" device(s) ready\n");
                        all_devices.extend(devices);
                    }
                    Err(e) => {
                        SerialPort::puts("[storage] init error: ");
                        SerialPort::puts(e);
                        SerialPort::puts("\n");
                    }
                }
            }
        }
    }
    all_devices
}
