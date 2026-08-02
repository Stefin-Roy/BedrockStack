//! USB class-driver registry.
//!
//! Mirrors `filesystems/blockdriver/driver.rs`: class drivers register
//! themselves in a static [`REGISTRY`], and the xHCI binding path
//! ([`super::super::xhci::bind_slot`]) asks the registry which driver
//! matches a configured interface via [`find_driver`], then hands the
//! driver the interface's endpoint resources through
//! [`UsbClassDriver::init_interface`].

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::services::dma::DmaAllocator;
use crate::usb::xhci::memory::TrbRing;

/// A single transfer endpoint of a configured interface, as handed to a
/// class driver.  The xHCI layer has already configured the endpoint and
/// created its transfer ring; the driver only needs to ring the doorbell
/// and wait for completions.
pub struct EndpointResource {
    pub dci: u8,
    pub mps: u16,
    /// xHCI endpoint-context Interval value (already converted per
    /// spec Table 6-12), not the raw USB `bInterval`.
    pub interval: u8,
    pub ring: TrbRing,
}

/// Everything a class driver needs to bind one interface of one slot.
pub struct InterfaceResources {
    pub slot_id: u8,
    pub doorbell_va: u64,
    /// The interface number (`wIndex` target for interface-scoped control
    /// requests such as HID `SET_PROTOCOL`).
    pub iface_num: u8,
    pub iface_class: u8,
    pub iface_subclass: u8,
    pub iface_protocol: u8,
    pub bulk_in: Option<EndpointResource>,
    pub bulk_out: Option<EndpointResource>,
    pub interrupt_in: Option<EndpointResource>,
}

/// The device a class driver binds.  Block devices are registered with the
/// block layer by the caller; input devices are registered directly with
/// UInputL inside `init_interface` (the `u32` is the UInputL-owned id).
pub enum BoundUsbDevice {
    Block(Arc<dyn BlockDevice>),
    Input(u32),
}

/// A USB class driver.  `probe` runs after interface configuration and
/// decides by interface class/subclass/protocol; `init_interface` binds the
/// interface, consuming the endpoint rings it needs.
pub trait UsbClassDriver: Send + Sync {
    fn name(&self) -> &str;
    fn probe(&self, iface_class: u8, subclass: u8, protocol: u8) -> bool;
    /// Bind one configured interface.  `ep0_ring` is the slot's default
    /// control-endpoint ring, shared while the interface configures itself
    /// (HID uses it for `SET_PROTOCOL` and descriptor fetches); drivers that
    /// need no control traffic ignore it.
    fn init_interface(
        &self,
        res: InterfaceResources,
        dma: &dyn DmaAllocator,
        ep0_ring: &mut TrbRing,
    ) -> Result<BoundUsbDevice, &'static str>;
}

static REGISTRY: Mutex<Vec<&'static dyn UsbClassDriver>> = Mutex::new(Vec::new());
static REGISTERED: AtomicBool = AtomicBool::new(false);

pub fn register(driver: &'static dyn UsbClassDriver) {
    REGISTRY.lock().push(driver);
}

/// Register the statically compiled class drivers.  Idempotent — called
/// lazily by [`find_driver`] so the registry is self-contained and the
/// boot sequence needs no changes.
fn register_all() {
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    register(&super::mass_storage::MassStorageDriver);
    #[cfg(target_arch = "x86_64")]
    register(&super::hid::HidDriver);
}

/// Return the first driver whose `probe` matches the interface, or `None`.
pub fn find_driver(iface_class: u8, subclass: u8, protocol: u8) -> Option<&'static dyn UsbClassDriver> {
    register_all();
    let registry = REGISTRY.lock();
    registry
        .iter()
        .copied()
        .find(|d| d.probe(iface_class, subclass, protocol))
}
