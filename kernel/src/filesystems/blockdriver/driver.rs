use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use super::traits::BlockDevice;
use crate::obj::clients::DmaClient;
use crate::pci::PciDevice;

pub trait StorageDriver: Send + Sync {
    fn name(&self) -> &str;
    fn probe(&self, dev: &PciDevice) -> bool;
    fn init_controller(
        &self,
        dev: &PciDevice,
        dma: DmaClient,
    ) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str>;
}

static REGISTRY: Mutex<Vec<&'static dyn StorageDriver>> = Mutex::new(Vec::new());

pub static BLOCK_DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

pub fn register(driver: &'static dyn StorageDriver) {
    REGISTRY.lock().push(driver);
}

fn register_all() {
    #[cfg(target_arch = "x86_64")]
    register(&super::ahci::AhciDriver);
}

pub fn init_all(
    pci_devices: &[PciDevice],
) -> Vec<Arc<dyn BlockDevice>> {
    use crate::drivers::serial::SerialPort;

    register_all();

    let dma = DmaClient::driver_dma();
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
