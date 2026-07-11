pub mod paging;
pub mod serial;

pub struct Riscv64;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::KernelLayout;
use super::Arch;

impl Arch for Riscv64 {
    fn init() {
    }

    fn halt() {
        loop {
            core::hint::spin_loop();
        }
    }

    fn disable_interrupts() {
    }

    fn enable_interrupts() {
    }

    fn setup_virt_mem(
        _allocator: &mut BitmapAllocator,
        _layout: &KernelLayout,
        _stack_guard: u64,
        _fb_addr: u64,
        _fb_height: usize,
        _fb_stride: usize,
    ) {
    }
}
