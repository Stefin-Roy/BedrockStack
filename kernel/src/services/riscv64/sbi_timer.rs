use core::sync::atomic::{AtomicPtr, Ordering};

use crate::arch::riscv64::time;

use super::super::capability::Capability;
use super::super::timer::TimerProvider;

const TIMEBASE_HZ: u64 = 10_000_000;

fn read_time_ns() -> u64 {
    let ticks = time::read_time();
    ticks * 1_000_000_000 / TIMEBASE_HZ
}

static TICK_HANDLER: AtomicPtr<fn()> = AtomicPtr::new(core::ptr::null_mut());

pub fn tick() {
    let ptr = TICK_HANDLER.load(Ordering::Relaxed);
    if !ptr.is_null() {
        let handler: fn() = unsafe { core::mem::transmute(ptr) };
        handler();
    }
}

pub struct SbiTimer;

impl Capability for SbiTimer {
    fn name(&self) -> &str {
        "sbi-timer"
    }
}

impl TimerProvider for SbiTimer {
    fn now_ns(&self) -> u64 {
        read_time_ns()
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

static SBI_TIMER: SbiTimer = SbiTimer;

pub fn init() -> &'static dyn TimerProvider {
    &SBI_TIMER as &'static dyn TimerProvider
}
