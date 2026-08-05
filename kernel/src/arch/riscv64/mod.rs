pub mod paging;
pub mod sbi;
pub mod serial;
pub mod time;
pub mod trampoline;
pub mod trap;

pub struct Riscv64;

use core::arch::asm;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::platform::riscv_virt::plic;
use crate::KernelLayout;

impl Riscv64 {
    pub fn init() {
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: trap handler\n");
        trap::init();
        // Set BSP's APIC/hart ID before PLIC init so scontext() can read it.
        crate::smp::set_bsp_hardware_id(
            crate::platform::riscv_virt::plic::HART_ID.load(core::sync::atomic::Ordering::Relaxed) as u32
        );
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: PLIC\n");
        plic::init();
        // Initialise the universal timer as early as possible — before
        // interrupts are enabled.  The timebase comes from the DTB, falling
        // back to the 10 MHz QEMU riscv-virt default.
        if let Some(dtb) = crate::platform::riscv_virt::get_dtb_ptr() {
            let hz = crate::dtb::timebase_hz(dtb);
            if hz != 0 {
                crate::services::riscv64::riscv_timebase::set_timebase_hz(hz);
            } else {
                crate::drivers::serial::SerialPort::puts(
                    "[arch] riscv64: DTB timebase absent — using 10 MHz fallback\n",
                );
            }
        }
        crate::services::universal_timer::early_init(
            &crate::services::riscv64::riscv_timebase::RiscvTimebaseClocksource,
            &crate::services::riscv64::riscv_timebase::RiscvSbiClockevent,
        );
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init: enabling supervisor interrupts\n");
        unsafe {
            asm!("csrw sie, {}", in(reg) trap::MIE_SEIE | trap::MIE_SSIE | trap::MIE_STIE);
        }
        crate::drivers::serial::SerialPort::puts("[arch] riscv64 init done\n");
    }

    pub fn init_ap(_cpu_id: u32) {
        // Set up trap vector for this hart.
        trap::init();
        // S-mode interrupt enable in sie.
        unsafe {
            asm!("csrw sie, {}", in(reg) trap::MIE_SEIE | trap::MIE_SSIE | trap::MIE_STIE);
        }
    }

    pub fn halt() {
        unsafe { asm!("wfi"); }
    }

    pub fn disable_interrupts() {
        unsafe { asm!("csrci sstatus, 2"); }
    }

    pub fn enable_interrupts() {
        unsafe { asm!("csrsi sstatus, 2"); }
    }

    pub fn are_interrupts_enabled() -> bool {
        let stval: u64;
        unsafe { asm!("csrr {}, sstatus", out(reg) stval); }
        (stval & 2) != 0
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
