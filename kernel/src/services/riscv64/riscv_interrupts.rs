use core::sync::atomic::{AtomicPtr, Ordering};

use super::super::interrupts::InterruptManager;
use crate::platform::riscv_virt::plic;

const NUM_PLIC_SOURCES: usize = 127;
static PLIC_HANDLERS: [AtomicPtr<fn()>; NUM_PLIC_SOURCES] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NUM_PLIC_SOURCES];

pub fn dispatch_external() {
    let irq = plic::claim();
    if irq == 0 || (irq as usize) >= NUM_PLIC_SOURCES {
        return;
    }
    let ptr = PLIC_HANDLERS[irq as usize].load(Ordering::Acquire);
    if !ptr.is_null() {
        let handler: fn() = unsafe { core::mem::transmute(ptr) };
        handler();
    }
    plic::complete(irq);
}

pub struct RiscvInterrupts;

impl InterruptManager for RiscvInterrupts {
    fn register_handler(&self, vector: u8, handler: fn()) {
        if (vector as usize) < NUM_PLIC_SOURCES {
            PLIC_HANDLERS[vector as usize].store(handler as *mut fn(), Ordering::Release);
        }
    }

    fn unregister_handler(&self, vector: u8) {
        if (vector as usize) < NUM_PLIC_SOURCES {
            PLIC_HANDLERS[vector as usize].store(core::ptr::null_mut(), Ordering::Release);
        }
    }

    fn enable(&self, vector: u8) {
        plic::enable_irq(vector as u32);
    }

    fn disable(&self, vector: u8) {
        plic::disable_irq(vector as u32);
    }

    fn eoi(&self) {}
}

static RISCV_INTERRUPTS: RiscvInterrupts = RiscvInterrupts;

pub fn init() -> &'static dyn InterruptManager {
    &RISCV_INTERRUPTS as &'static dyn InterruptManager
}
