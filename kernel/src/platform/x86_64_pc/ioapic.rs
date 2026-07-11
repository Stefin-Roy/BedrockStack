//! I/O APIC driver — programs interrupt redirection entries.
//!
//! Each IOAPIC is accessed via two MMIO registers:
//!   IOREGSEL (offset 0x00) — write the desired register index
//!   IOWIN    (offset 0x10) — read/write the selected register's value
//!
//! Redirection entries (one per interrupt pin) live at indices
//!   0x10 + 2*i  (low  32 bits)
//!   0x10 + 2*i+1 (high 32 bits)

use spin::Mutex;

use crate::drivers::serial::SerialPort;
use crate::mm::vmm::PageFlags;

use acpi::platform::interrupt::{Polarity, TriggerMode};

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

static IOAPIC: Mutex<Option<IoApicState>> = Mutex::new(None);

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
fn map_ioapic_mmio(phys: u64) -> u64 {
    crate::acpi::map_device_mmio(phys, 4096, PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE)
}

/// Initialise the IOAPIC driver.
///
/// Maps the IOAPIC MMIO region, reads version/entry count, and masks all
/// redirection entries so no stray interrupts fire before we set them up.
pub fn init(phys_base: u64, gsi_base: u32) {
    let vaddr = map_ioapic_mmio(phys_base);

    let state = IoApicState {
        base_virt: vaddr,
        entries: 0,
        gsi_base,
        next_vector: 33,
    };

    let ver = ioapic_read(&state, REG_IOAPIC_VER);
    let entries = (ver >> 16) & 0xFF;
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

    ioapic_write(state, index, low);
    ioapic_write(state, index + 1, high);

    SerialPort::puts("[ioapic] enabled GSI ");
    SerialPort::put_u64(gsi as u64);
    SerialPort::puts(" → vector ");
    SerialPort::put_u64(vector as u64);
    SerialPort::puts("\n");

    Some(vector)
}

/// Mask (disable) a GSI.
pub fn mask_irq(gsi: u32) {
    let mut guard = IOAPIC.lock();
    let state = guard.as_mut().expect("IOAPIC not initialized");
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
    let state = guard.as_mut().expect("IOAPIC not initialized");
    if gsi < state.gsi_base || gsi >= state.gsi_base + state.entries {
        return;
    }
    let index = 0x10 + 2 * (gsi - state.gsi_base);
    let low = ioapic_read(state, index);
    ioapic_write(state, index, low & !(REDIR_MASK as u32));
}
