use core::ptr::{read_volatile, write_volatile};

const PLIC_BASE: u64 = 0x0C000000;
const PLIC_PRIORITY: u64 = 0x000000;
const PLIC_PENDING: u64 = 0x001000;
const PLIC_ENABLE: u64 = 0x002000;
const PLIC_CONTEXT: u64 = 0x200000;

const PRIORITY_STRIDE: u64 = 4;
const WORD_STRIDE: u64 = 4;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_STRIDE: u64 = 0x1000;

const CONTEXT_THRESHOLD: u64 = 0x00;
const CONTEXT_CLAIM: u64 = 0x04;

// QEMU riscv-virt has 127 interrupt sources; PLIC spec allows up to 1023.
const NUM_SOURCES: usize = 127;

fn priority_addr(irq: u32) -> *mut u32 {
    (PLIC_BASE + PLIC_PRIORITY + irq as u64 * PRIORITY_STRIDE) as *mut u32
}

fn pending_addr(word: usize) -> *mut u32 {
    (PLIC_BASE + PLIC_PENDING + word as u64 * WORD_STRIDE) as *mut u32
}

fn enable_addr(context: usize, word: usize) -> *mut u32 {
    (PLIC_BASE + PLIC_ENABLE + context as u64 * ENABLE_STRIDE + word as u64 * WORD_STRIDE) as *mut u32
}

fn context_threshold(context: usize) -> *mut u32 {
    (PLIC_BASE + PLIC_CONTEXT + context as u64 * CONTEXT_STRIDE + CONTEXT_THRESHOLD) as *mut u32
}

fn context_claim(context: usize) -> *mut u32 {
    (PLIC_BASE + PLIC_CONTEXT + context as u64 * CONTEXT_STRIDE + CONTEXT_CLAIM) as *mut u32
}

/// Initialise the PLIC.
///
/// Sets all interrupt priorities to 0 (disabled) and clears threshold.
/// Must be called once during platform init.
pub fn init() {
    // Disable all interrupts by setting priority to 0.
    for irq in 1..=NUM_SOURCES {
        unsafe { write_volatile(priority_addr(irq as u32), 0); }
    }

    // Clear all enable bits for our context.
    // QEMU virt: hart 0 S-mode context = (0 * 2) + 1 = 1
    let context = scontext();
    for word in 0..4 {
        unsafe { write_volatile(enable_addr(context, word), 0); }
    }

    // Set threshold to 0 (accept all).
    unsafe { write_volatile(context_threshold(context), 0); }
}

/// Enable a specific interrupt source for S-mode.
pub fn enable_irq(irq: u32) {
    let context = scontext();
    let word = (irq as usize) / 32;
    let bit = (irq as usize) % 32;
    unsafe {
        let addr = enable_addr(context, word);
        let val = read_volatile(addr);
        write_volatile(addr, val | (1 << bit));
    }
}

/// Disable a specific interrupt source.
pub fn disable_irq(irq: u32) {
    let context = scontext();
    let word = (irq as usize) / 32;
    let bit = (irq as usize) % 32;
    unsafe {
        let addr = enable_addr(context, word);
        let val = read_volatile(addr);
        write_volatile(addr, val & !(1 << bit));
    }
}

/// Set priority for an interrupt source (1-7, where 7 is highest).
pub fn set_priority(irq: u32, priority: u32) {
    unsafe { write_volatile(priority_addr(irq), priority & 7); }
}

/// Claim the highest-priority pending interrupt.
///
/// Returns the IRQ number, or 0 if none is pending.
pub fn claim() -> u32 {
    let context = scontext();
    unsafe { read_volatile(context_claim(context)) }
}

/// Complete (EOI) an interrupt.
pub fn complete(irq: u32) {
    let context = scontext();
    unsafe { write_volatile(context_claim(context), irq); }
}

/// Return the S-mode PLIC context for the current hart.
///
/// QEMU riscv-virt provides 2 contexts per hart (M-mode and S-mode).
/// Context = hart_id * 2 + 1 for S-mode.
fn scontext() -> usize {
    let hart: u64;
    unsafe { core::arch::asm!("csrr {}, mhartid", out(reg) hart); }
    (hart as usize) * 2 + 1
}
