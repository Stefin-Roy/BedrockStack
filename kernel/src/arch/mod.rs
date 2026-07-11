use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::Vmm;
use crate::KernelLayout;

/// Architecture-specific operations.
///
/// Each supported target (x86_64, riscv64, …) provides an implementation
/// behind the `CurrentArch` alias so the rest of the kernel is
/// architecture-agnostic.
pub trait Arch {
    /// Early architecture initialisation (GDT+IDT on x86, trap vectors on
    /// RISC-V, etc.).
    fn init();

    /// Halt the CPU.  May return after an interrupt or NMI.
    fn halt();

    /// Disable interrupts.
    fn disable_interrupts();

    /// Enable interrupts.
    fn enable_interrupts();

    /// Build page tables with identity-mapped RAM and a higher-half kernel
    /// alias at `KERNEL_VMA_BASE + phys`.
    ///
    /// Returns the `Vmm` (page-table root) **without** activating it — the
    /// caller is responsible for `Vmm::activate()` after.
    ///
    /// # Safety
    /// - `allocator` is initialised and has free frames.
    fn setup_virt_mem(
        allocator: &mut BitmapAllocator,
        layout: &KernelLayout,
        stack_guard: u64,
        fb_addr: u64,
        fb_height: usize,
        fb_stride: usize,
    ) -> Vmm;
}

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::X86_64 as CurrentArch;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::Riscv64 as CurrentArch;
