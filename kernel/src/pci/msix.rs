use core::ptr::{read_volatile, write_volatile};
use crate::filesystems::vfs::irq::IrqMutex;

use super::PciDevice;
use super::bar::Bar;
use super::caps::{self, PciCapability};
use crate::drivers::serial::SerialPort;

fn cfg() -> &'static dyn crate::services::pci_config::PciConfigSpace {
    crate::services::kernel_services().pci_cfg
}

/// Diagnostic snapshot of the last programmed MSI-X entry.
pub struct MsixDiag {
    pub table_va: u64,
    pub pba_va: u64,
}
static MSIX_DIAG: IrqMutex<Option<MsixDiag>> = IrqMutex::new(None);

/// Store diagnostic addresses for the MSI-X table and PBA.
pub fn set_diag_addrs(table_va: u64, pba_va: u64) {
    *MSIX_DIAG.lock() = Some(MsixDiag { table_va, pba_va });
}

/// Read back entry 0's msg_addr from the diagnosed table.
/// Returns `None` when no MSI-X table has been diagnosed (the caller must
/// not confuse that with a genuinely programmed address of 0).
pub fn diag_read_addr() -> Option<u64> {
    let guard = MSIX_DIAG.lock();
    let d = guard.as_ref()?;
    unsafe {
        let lo = read_volatile(d.table_va as *const u32);
        let hi = read_volatile((d.table_va + 4) as *const u32);
        Some((lo as u64) | ((hi as u64) << 32))
    }
}

/// Read back entry 0's msg_data from the diagnosed table.
pub fn diag_read_data() -> Option<u32> {
    let guard = MSIX_DIAG.lock();
    let d = guard.as_ref()?;
    unsafe { Some(read_volatile((d.table_va + 8) as *const u32)) }
}

/// Read back entry 0's vector control from the diagnosed table.
pub fn diag_read_vc() -> Option<u32> {
    let guard = MSIX_DIAG.lock();
    let d = guard.as_ref()?;
    unsafe { Some(read_volatile((d.table_va + 12) as *const u32)) }
}

/// Read the PBA word for entry 0.
pub fn diag_read_pba() -> Option<u32> {
    let guard = MSIX_DIAG.lock();
    let d = guard.as_ref()?;
    unsafe { Some(read_volatile(d.pba_va as *const u32)) }
}

/// MSI-X Message Control register bits (capability offset +2).
const MC_MSIX_ENABLE: u16 = 1 << 15;
const MC_FUNCTION_MASK: u16 = 1 << 14;
const PCI_COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const PCI_COMMAND_BUS_MASTER: u16 = 1 << 2;
const PCI_COMMAND_INTERRUPT_DISABLE: u16 = 1 << 10;

/// MSI-X table entry (16 bytes, in device BAR space).
#[repr(C)]
struct MsixTableEntry {
    msg_addr_lo: u32,
    msg_addr_hi: u32,
    msg_data: u32,
    vector_ctrl: u32,
}

/// Information parsed from the MSI-X capability.
pub struct MsixInfo {
    pub table_size: u16,
    pub bir: usize,
    pub table_offset: u64,
    pub pba_bir: usize,
    pub pba_offset: u64,
}

/// Parse the MSI-X capability to extract table and PBA location.
pub fn table_info(dev: &PciDevice, cap: &PciCapability) -> MsixInfo {
    let mc = caps::read_u16(dev, cap, 2);
    // Table Size is Message Control bits 0..=10 and is encoded as N - 1.
    // (Bits 14 and 15 are Enable and Function Mask respectively.)
    let table_size = (mc & 0x07FF) + 1;

    let tbl = caps::read_u32(dev, cap, 4);
    let bir = (tbl & 0x7) as usize;
    let table_offset = (tbl & 0xFFFF_FFF8) as u64;

    let pba = caps::read_u32(dev, cap, 8);
    let pba_bir = (pba & 0x7) as usize;
    let pba_offset = (pba & 0xFFFF_FFF8) as u64;

    MsixInfo {
        table_size,
        bir,
        table_offset,
        pba_bir,
        pba_offset,
    }
}

/// Enable MSI-X for a device.
///
/// `bar_va` is the virtual address of the mapped BAR that contains the
/// MSI-X table (the BAR index is read from the MSI-X capability).
/// `pba_va` is the virtual address of the mapped BAR that contains the
/// Pending Bit Array (its BAR index is `info.pba_bir`).  When the table
/// and PBA share a BAR (common), pass the same address for both.
pub fn enable(
    dev: &PciDevice,
    cap: &PciCapability,
    bar_va: u64,
    pba_va: u64,
    table_entries: u16,
    vector: u8,
    dest_apic_id: u8,
) {
    if table_entries == 0 {
        SerialPort::puts("[msix] refusing to enable with no table entries\n");
        return;
    }

    let info = table_info(dev, cap);

    // Validate the table's BAR is memory-mapped.
    match super::bar::bar(dev, info.bir) {
        Bar::Memory { .. } => {}
        _ => {
            SerialPort::puts("[msix] table BAR is not memory-mapped, cannot enable\n");
            return;
        }
    }

    // Validate the PBA's BAR is memory-mapped.
    match super::bar::bar(dev, info.pba_bir) {
        Bar::Memory { .. } => {}
        _ => {
            SerialPort::puts("[msix] PBA BAR is not memory-mapped, cannot enable\n");
            return;
        }
    }

    // Store diagnostic addresses so snapshot functions work immediately.
    let table_va = bar_va + info.table_offset;
    let pba_va_full = pba_va + info.pba_offset;
    set_diag_addrs(table_va, pba_va_full);

    let mc = caps::read_u16(dev, cap, 2);

    // Keep the function masked until the table and the PCI command register
    // are complete.  An xHC can otherwise signal a stale table entry as soon
    // as MSI-X is enabled.
    caps::write_u16(dev, cap, 2, (mc & !MC_MSIX_ENABLE) | MC_FUNCTION_MASK);

    let addr: u64 = 0xFEE00000 | ((dest_apic_id as u64) << 12);
    let addr_lo = addr as u32;
    let addr_hi = (addr >> 32) as u32;
    let data: u32 = vector as u32;
    let count = table_entries.min(info.table_size);

    let table = table_va as *mut MsixTableEntry;

    // Mask every entry (preserving reserved bits) before touching message
    // fields.  This is required even with Function Mask set.
    for i in 0..info.table_size as usize {
        unsafe {
            let vc = read_volatile(&(*table.add(i)).vector_ctrl);
            write_volatile(&mut (*table.add(i)).vector_ctrl, vc | VECTOR_CTRL_MASK);
        }
    }

    // Program the first `count` table entries using 32-bit writes as
    // required by the PCI specification.
    for i in 0..count as usize {
        unsafe {
            write_volatile(&mut (*table.add(i)).msg_addr_lo, addr_lo);
            write_volatile(&mut (*table.add(i)).msg_addr_hi, addr_hi);
            write_volatile(&mut (*table.add(i)).msg_data, data);
        }
    }

    // Ensure table writes are visible before we touch config space.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence", options(nostack, preserves_flags));
    }

    SerialPort::puts("[msix] enabled: vector=");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts(" entries=");
    SerialPort::put_u64(count as u64);
    SerialPort::puts("\n");

    // The device must be allowed to access its BAR and DMA the event ring.
    // Firmware usually enables these bits, but relying on that made MSI-X
    // setup boot-order dependent.  INTx remains disabled because it is
    // mutually exclusive with MSI-X.
    let pci_cfg = cfg();
    let cmd = pci_cfg.read16(dev.segment, dev.bus, dev.device, dev.function, 0x04);
    pci_cfg.write16(
        dev.segment,
        dev.bus,
        dev.device,
        dev.function,
        0x04,
        cmd | PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER | PCI_COMMAND_INTERRUPT_DISABLE,
    );

    // Enable MSI-X (keep Function Mask set).
    let current_mc = caps::read_u16(dev, cap, 2);
    caps::write_u16(dev, cap, 2, current_mc | MC_MSIX_ENABLE);

    // Unmask the programmed entries (preserving reserved bits), then
    // release Function Mask.  Keep this ordering: an unmasked entry must
    // never precede MSI-X enable.
    for i in 0..count as usize {
        unsafe {
            let vc = read_volatile(&(*table.add(i)).vector_ctrl);
            write_volatile(&mut (*table.add(i)).vector_ctrl, vc & !VECTOR_CTRL_MASK);
        }
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence", options(nostack, preserves_flags));
    }
    let enabled_mc = caps::read_u16(dev, cap, 2);
    caps::write_u16(dev, cap, 2, enabled_mc & !MC_FUNCTION_MASK);
}

/// Disable MSI-X for a device.
pub fn disable(dev: &PciDevice, cap: &PciCapability) {
    let mc = caps::read_u16(dev, cap, 2);
    caps::write_u16(dev, cap, 2, mc & !MC_MSIX_ENABLE);
}

/// Mask bit in the MSI-X table entry Vector Control field.
const VECTOR_CTRL_MASK: u32 = 1 << 0;

/// Program a single MSI-X table entry with the given vector and target APIC.
///
/// The entry is masked before writing and unmasked after, so no spurious
/// interrupt fires during programming.  Use this when different entries
/// need different vectors (e.g. NVMe per-queue MSI-X).
pub fn program_entry(
    dev: &PciDevice,
    cap: &PciCapability,
    bar_va: u64,
    entry_index: u16,
    vector: u8,
    dest_apic_id: u8,
) {
    let info = table_info(dev, cap);
    if entry_index >= info.table_size {
        SerialPort::puts("[msix] program_entry: index out of range\n");
        return;
    }

    let table_va = bar_va + info.table_offset;
    unsafe {
        let entry = (table_va as *mut MsixTableEntry).add(entry_index as usize);
        // Mask the entry first (preserving reserved bits).
        let vc = read_volatile(&(*entry).vector_ctrl);
        write_volatile(&mut (*entry).vector_ctrl, vc | VECTOR_CTRL_MASK);
        let addr: u64 = 0xFEE00000 | ((dest_apic_id as u64) << 12);
        let data: u32 = vector as u32;
        // Use 32-bit writes as required by the PCI specification.
        write_volatile(&mut (*entry).msg_addr_lo, addr as u32);
        write_volatile(&mut (*entry).msg_addr_hi, (addr >> 32) as u32);
        write_volatile(&mut (*entry).msg_data, data);
        // Ensure message address/data are visible before unmasking.
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("mfence", options(nostack, preserves_flags));
        #[cfg(target_arch = "riscv64")]
        core::arch::asm!("fence", options(nostack, preserves_flags));
        // Unmask (preserving reserved bits).
        let vc = read_volatile(&(*entry).vector_ctrl);
        write_volatile(&mut (*entry).vector_ctrl, vc & !VECTOR_CTRL_MASK);
    }

    SerialPort::puts("[msix] entry=");
    SerialPort::put_u64(entry_index as u64);
    SerialPort::puts(" vector=");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts("\n");
}

/// Mask a single MSI-X table entry so the device will not generate
/// an interrupt on that vector.
pub fn mask_entry(dev: &PciDevice, cap: &PciCapability, bar_va: u64, entry_index: u16) {
    let info = table_info(dev, cap);
    if entry_index >= info.table_size {
        return;
    }
    let table_va = bar_va + info.table_offset;
    unsafe {
        let entry = (table_va as *mut MsixTableEntry).add(entry_index as usize);
        let vc = read_volatile(&(*entry).vector_ctrl);
        write_volatile(&mut (*entry).vector_ctrl, vc | VECTOR_CTRL_MASK);
    }
}

/// Unmask a single MSI-X table entry, re-enabling interrupt delivery.
pub fn unmask_entry(dev: &PciDevice, cap: &PciCapability, bar_va: u64, entry_index: u16) {
    let info = table_info(dev, cap);
    if entry_index >= info.table_size {
        return;
    }
    let table_va = bar_va + info.table_offset;
    unsafe {
        let entry = (table_va as *mut MsixTableEntry).add(entry_index as usize);
        let vc = read_volatile(&(*entry).vector_ctrl);
        write_volatile(&mut (*entry).vector_ctrl, vc & !VECTOR_CTRL_MASK);
    }
}

/// Read the Pending Bit Array for a given entry index.
/// Returns true if there is a pending interrupt for entry `index`.
///
/// `pba_va` is the mapped virtual address of the PBA within the device's BAR.
pub fn pending(pba_va: u64, index: u16) -> bool {
    let word = (index / 32) as usize;
    let bit = index % 32;
    unsafe {
        let pba = pba_va as *const u32;
        read_volatile(pba.add(word)) & (1u32 << bit) != 0
    }
}
