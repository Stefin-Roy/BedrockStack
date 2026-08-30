//! I/O APIC driver — programs interrupt redirection entries.
//!
//! Each IOAPIC is accessed via two MMIO registers:
//!   IOREGSEL (offset 0x00) — write the desired register index
//!   IOWIN    (offset 0x10) — read/write the selected register's value
//!
//! Redirection entries (one per interrupt pin) live at indices
//!   0x10 + 2*i  (low  32 bits)
//!   0x10 + 2*i+1 (high 32 bits)

use crate::filesystems::vfs::irq::IrqMutex;

use crate::drivers::serial::SerialPort;
use crate::mm::vmm::PageFlags;

use crate::acpi::{Polarity, TriggerMode};

const REG_IOAPIC_VER: u32 = 0x01;
const REDIR_MASK: u64 = 1 << 16;
const REDIR_POLARITY: u64 = 1 << 13;
const REDIR_TRIGGER: u64 = 1 << 15;

struct IoApicState {
    base_virt: u64,
    entries: u32,
    gsi_base: u32,
    next_vector: u8,
}

static IOAPIC: IrqMutex<Option<IoApicState>> = IrqMutex::new(None);

fn ioapic_write(state: &IoApicState, reg: u32, val: u32) {
    let base = state.base_virt as *mut u32;
    unsafe {
        base.add(0).write_volatile(reg);
        base.add(4).write_volatile(val);
    }
}

fn ioapic_read(state: &IoApicState, reg: u32) -> u32 {
    let base = state.base_virt as *mut u32;
    unsafe {
        base.add(0).write_volatile(reg);
        base.add(4).read_volatile()
    }
}

/// Map the IOAPIC physical MMIO region into the virtual address space.
/// On VMM exhaustion (malformed MADT) returns 0 sentinel; caller `init` will
/// then create a state that reads as all-masked (no IRQs) rather than abort.
fn map_ioapic_mmio(phys: u64) -> u64 {
    crate::acpi::try_map_device_mmio(
        phys,
        4096,
        PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE,
    )
    .unwrap_or_else(|e| {
        log::error!("IOAPIC VMM exhaustion: {} (phys={:#x})", e, phys);
        0
    })
}

/// Initialise the IOAPIC driver.
///
/// Maps the IOAPIC MMIO region, reads version/entry count, and masks all
/// redirection entries so no stray interrupts fire before we set them up.
pub fn init(phys_base: u64, gsi_base: u32) {
    let vaddr = map_ioapic_mmio(phys_base);
    if vaddr == 0 {
        log::error!("IOAPIC init failed: VMM mapping returned 0 for phys {:#x}", phys_base);
        return;
    }

    let state = IoApicState {
        base_virt: vaddr,
        entries: 0,
        gsi_base,
        next_vector: 33,
    };

    let ver = ioapic_read(&state, REG_IOAPIC_VER);
    let max_entry = (ver >> 16) & 0xFF;
    let entries = max_entry + 1;
    let state = IoApicState { entries, ..state };

    SerialPort::puts("[ioapic] base=0x");
    SerialPort::put_hex(phys_base);
    SerialPort::puts(" gsi_base=");
    SerialPort::put_u64(gsi_base as u64);
    SerialPort::puts(" entries=");
    SerialPort::put_u64(entries as u64);
    SerialPort::puts("\n");

    // Mask all entries initially
    for i in 0..entries {
        let low = ioapic_read(&state, 0x10 + 2 * i);
        ioapic_write(&state, 0x10 + 2 * i, low | REDIR_MASK as u32);
    }

    *IOAPIC.lock() = Some(state);
}

/// Program a redirection entry for a GSI.
///
/// Returns the interrupt vector assigned, or `None` if this IOAPIC doesn't
/// manage the given GSI.
pub fn enable_irq(gsi: u32, polarity: Polarity, trigger: TriggerMode) -> Option<u8> {
    let mut guard = IOAPIC.lock();
    let state = guard.as_mut()?;

    if gsi < state.gsi_base || gsi >= state.gsi_base + state.entries {
        return None;
    }

    let index = 0x10 + 2 * (gsi - state.gsi_base);
    let vector = state.next_vector;
    if vector >= crate::arch::x86_64::idt::DEVICE_VECTOR_END
        || vector < crate::arch::x86_64::idt::DEVICE_VECTOR_BASE
    {
        SerialPort::puts("[ioapic] WARN: interrupt vectors exhausted, cannot enable GSI ");
        SerialPort::put_u64(gsi as u64);
        SerialPort::puts(" (allocated ");
        SerialPort::put_u64(crate::arch::x86_64::idt::allocated_device_vectors() as u64);
        SerialPort::puts("/");
        SerialPort::put_u64(crate::arch::x86_64::idt::NUM_DEVICE_VECTORS as u64);
        SerialPort::puts(")\n");
        return None;
    }
    state.next_vector += 1;

    let mut low = vector as u32;
    if polarity == Polarity::ActiveLow {
        low |= REDIR_POLARITY as u32;
    }
    if trigger == TriggerMode::Level {
        low |= REDIR_TRIGGER as u32;
    }
    low &= !(REDIR_MASK as u32);

    let high: u32 = 0;

    // Per Intel IOAPIC spec: write high DWORD first, then low DWORD
    // (low DWORD write triggers the update).
    ioapic_write(state, index + 1, high);
    ioapic_write(state, index, low);

    SerialPort::puts("[ioapic] enabled GSI ");
    SerialPort::put_u64(gsi as u64);
    SerialPort::puts(" -> vector ");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts("\n");

    Some(vector)
}

/// Mask (disable) a GSI.
pub fn mask_irq(gsi: u32) {
    let mut guard = IOAPIC.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };
    if gsi < state.gsi_base || gsi >= state.gsi_base + state.entries {
        return;
    }
    let index = 0x10 + 2 * (gsi - state.gsi_base);
    let low = ioapic_read(state, index);
    ioapic_write(state, index, low | REDIR_MASK as u32);
}

/// Unmask (enable) a GSI.
pub fn unmask_irq(gsi: u32) {
    let mut guard = IOAPIC.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };
    if gsi < state.gsi_base || gsi >= state.gsi_base + state.entries {
        return;
    }
    let index = 0x10 + 2 * (gsi - state.gsi_base);
    let low = ioapic_read(state, index);
    ioapic_write(state, index, low & !(REDIR_MASK as u32));
}

/// Program a GSI to deliver as NMI (delivery 100, vector ignored).
/// Used by the watchdog PIT fallback — the PIT’s GSI 0 fires as NMI
/// even when IF=0, so a hung spin with interrupts disabled still
/// enters `nmi_handler` and dumps the interrupted RIP.
pub fn enable_nmi(gsi: u32, polarity: Polarity, trigger: TriggerMode) -> bool {
    let mut guard = IOAPIC.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return false,
    };
    if gsi < state.gsi_base || gsi >= state.gsi_base + state.entries {
        return false;
    }
    let index = 0x10 + 2 * (gsi - state.gsi_base);
    // Delivery NMI = 0b100 << 8, vector field ignored.
    let mut low: u32 = 0b100 << 8;
    if polarity == Polarity::ActiveLow {
        low |= REDIR_POLARITY as u32;
    }
    if trigger == TriggerMode::Level {
        low |= REDIR_TRIGGER as u32;
    }
    low &= !(REDIR_MASK as u32);
    // Destination: physical, BSP APIC ID 0 (QEMU smp 4 ids 0..3).
    // NMI is maskable only by LVT mask, not IF — so IF=0 hangs still NMI.
    let high: u32 = 0; // APIC ID 0 << 24
    ioapic_write(state, index + 1, high);
    ioapic_write(state, index, low);
    SerialPort::puts("[ioapic] enabled GSI ");
    SerialPort::put_u64(gsi as u64);
    SerialPort::puts(" as NMI\n");
    true
}
