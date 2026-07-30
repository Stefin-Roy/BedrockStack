use core::mem;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::super::capability::Capability;
use super::super::timer::TimerProvider;

static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

static TICK_HANDLER: AtomicPtr<fn()> = AtomicPtr::new(core::ptr::null_mut());

pub fn tick() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    let ptr = TICK_HANDLER.load(Ordering::Relaxed);
    if !ptr.is_null() {
        let handler: fn() = unsafe { mem::transmute(ptr) };
        handler();
    }
}

pub struct ApicTimer;

impl Capability for ApicTimer {
    fn name(&self) -> &str {
        "apic-timer"
    }
}

impl TimerProvider for ApicTimer {
    fn now_ns(&self) -> u64 {
        TICK_COUNT.load(Ordering::Relaxed) * 1_000_000
    }

    fn sleep_ns(&self, ns: u64) {
        let deadline = self.now_ns() + ns;
        while self.now_ns() < deadline {
            core::hint::spin_loop();
        }
    }

    fn register_tick_handler(&self, handler: fn()) {
        TICK_HANDLER.store(handler as *mut fn(), Ordering::Release);
    }
}

static APIC_TIMER: ApicTimer = ApicTimer;

pub fn init() -> &'static dyn TimerProvider {
    crate::arch::x86_64::idt::set_timer_tick_callback(tick);
    &APIC_TIMER as &'static dyn TimerProvider
}
