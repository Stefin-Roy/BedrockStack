use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use super::dma::DmaAllocator;
use super::traits::BlockDevice;
use crate::mm::vmm::KERNEL_VMA_BASE;
use crate::pci::PciDevice;

pub trait StorageDriver: Send + Sync {
    fn name(&self) -> &str;
    fn probe(&self, dev: &PciDevice) -> bool;
    fn init_controller(
        &self,
        dev: &PciDevice,
        dma: &mut DmaAllocator,
    ) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str>;
}

static REGISTRY: Mutex<Vec<&'static dyn StorageDriver>> = Mutex::new(Vec::new());

pub fn register(driver: &'static dyn StorageDriver) {
    REGISTRY.lock().push(driver);
}

fn register_all() {
    #[cfg(target_arch = "x86_64")]
    register(&super::ahci::AhciDriver);
}

const VMM_VADDR: u64 = KERNEL_VMA_BASE - 0x10000000 - 0x20000000 - 0x20000000;
const VMM_VADDR_FLOOR: u64 = VMM_VADDR - 0x2000_0000;

pub fn init_all(
    pci_devices: &[PciDevice],
    root: u64,
    alloc: *mut crate::mm::phys_alloc::BitmapAllocator,
) -> Vec<Arc<dyn BlockDevice>> {
    use crate::drivers::serial::SerialPort;

    register_all();

    let mut dma = DmaAllocator::new(root, alloc, VMM_VADDR, VMM_VADDR_FLOOR);
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
                match driver.init_controller(dev, &mut dma) {
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
