pub mod gdt;
pub mod idt;
#[cfg(feature = "cpu_slow")]
pub mod limiter;
pub mod paging;
pub mod serial;
pub mod syscall;
pub mod trampoline;
#[cfg(feature = "kernelmb2")]
mod multiboot2;

use crate::platform::x86_64_pc::apic;
use crate::services::clockevent::Clockevent;
use crate::services::clocksource::Clocksource;
use crate::services::universal_timer;

pub struct X86_64;

use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::KernelLayout;

impl X86_64 {
    pub fn init() {
        SerialPort::puts("[arch] x86_64 init: GDT\n");
        gdt::init();
        SerialPort::puts("[arch] x86_64 init: IDT\n");
        idt::init();
        SerialPort::puts("[arch] x86_64 init: APIC\n");
        apic::init();
        // Record the BSP's APIC ID after APIC init.
        crate::smp::set_bsp_hardware_id(apic::read_full_apic_id());

        // Initialise the universal timer as early as possible — before
        // services, before SMP, before interrupts are enabled.
        universal_timer::early_init(
            &X86TscClocksource,
            &ApicOneShotClockevent,
        );
    }

    pub fn init_ap(_cpu_id: u32) {
        crate::arch::x86_64::gdt::init();
        crate::arch::x86_64::idt::init_ap();
        crate::platform::x86_64_pc::apic::init_ap();
    }

    pub fn halt() {
        x86_64::instructions::hlt();
    }

    pub fn disable_interrupts() {
        x86_64::instructions::interrupts::disable();
    }

    pub fn enable_interrupts() {
        x86_64::instructions::interrupts::enable();
    }

    pub fn are_interrupts_enabled() -> bool {
        x86_64::instructions::interrupts::are_enabled()
    }

    pub fn setup_virt_mem(
        allocator: &mut BitmapAllocator,
        layout: &KernelLayout,
        stack_guard: u64,
        fb_addr: u64,
        fb_height: usize,
        fb_stride: usize,
        fb_bpp: u8,
    ) -> Vmm {
        paging::setup(allocator, layout, stack_guard, fb_addr, fb_height, fb_stride, fb_bpp)
    }
}

// ── TSC clocksource ───────────────────────────────────────────────

pub struct X86TscClocksource;



impl Clocksource for X86TscClocksource {
    fn now_ns(&self) -> u64 {
        apic::tsc_now_ns()
    }
}

// ── APIC one-shot clockevent ──────────────────────────────────────
//
// Converts absolute nanosecond deadlines into APIC timer ticks and
// programs the LAPIC in one-shot mode.

pub struct ApicOneShotClockevent;



impl Clockevent for ApicOneShotClockevent {
    /// Program the APIC timer to fire at (or slightly after) `deadline_ns`.
    ///
    /// The APIC timer is programmed as a one-shot.  The actual interrupt
    /// may arrive slightly late due to APIC tick granularity, but never
    /// before the requested deadline.
    fn set_deadline(&self, deadline_ns: u64) {
        let now = apic::tsc_now_ns();
        if deadline_ns <= now {
            // Already past — fire as soon as possible (minimum 1 tick).
            apic::oneshot_timer_set(1);
            return;
        }
        let delta_ns = deadline_ns - now;
        let apic_hz = apic::apic_hz();
        if apic_hz == 0 {
            // Not calibrated (shouldn't happen) — skip.
            return;
        }
        // delta_ns * apic_hz / 1_000_000_000, avoiding overflow.
        let count = if delta_ns < 1_000_000_000 {
            (delta_ns * apic_hz) / 1_000_000_000
        } else {
            (delta_ns / 1_000_000_000) * apic_hz
        };
        apic::oneshot_timer_set(core::cmp::max(1, count as u32));
    }

    fn stop(&self) {
        apic::timer_stop();
    }
}
