use crate::mm::phys_alloc::BitmapAllocator;
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

    /// Build new identity-mapped page tables and switch to them.
    ///
    /// # Safety
    /// - `allocator` is initialised and has free frames.
    /// - After this call the current instruction and stack must remain
    ///   identity-mapped (which holds because all RAM is mapped).
    fn setup_virt_mem(
        allocator: &mut BitmapAllocator,
        layout: &KernelLayout,
        stack_guard: u64,
        fb_addr: u64,
        fb_height: usize,
        fb_stride: usize,
    );
}

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::X86_64 as CurrentArch;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::Riscv64 as CurrentArch;
