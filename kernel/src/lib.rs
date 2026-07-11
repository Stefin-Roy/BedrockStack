#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
extern crate alloc;

pub mod acpi;
pub mod arch;
pub mod boot;
pub mod display;
pub mod drivers;
pub mod acpi_log;
pub mod mm;
pub mod module;
pub mod platform;

use acpi::AcpiSubsystem;
use arch::{Arch, CurrentArch};
use boot::{FramebufferInfo, MemoryRegion};
use display::framebuffer::Framebuffer;

use mm::heap;
use mm::phys_alloc::BitmapAllocator;
use module::registry::init_all;

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rela_dyn_start: u8;
    static __rela_dyn_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
}

/// Physical-address boundaries of the kernel image sections, used to apply
/// W^X permissions when building the page tables.
#[derive(Clone, Copy)]
pub struct KernelLayout {
    pub kernel_start: u64,
    pub kernel_end: u64,
    pub text_start: u64,
    pub text_end: u64,
    pub rela_dyn_start: u64,
    pub rela_dyn_end: u64,
    pub rodata_start: u64,
    pub rodata_end: u64,
}

pub struct Kernel {
    framebuffer: Framebuffer,
    allocator: BitmapAllocator,
    layout: KernelLayout,
    stack_guard: u64,
    #[allow(dead_code)]
    memory_map: &'static [MemoryRegion],
    acpi: Option<AcpiSubsystem>,
}

impl Kernel {
    /// # Safety
    /// memory_map must be a valid slice of MemoryRegion.
    /// framebuffer must be a valid reference to data collected before exit_boot_services.
    /// stack_guard is the physical address of the stack guard page to leave unmapped.
    pub unsafe fn new(
        memory_map: &'static [MemoryRegion],
        framebuffer: &FramebufferInfo,
        stack_guard: u64,
        rsdp_addr: u64,
    ) -> Self {
        crate::acpi_log::init();

        let display = unsafe {
            Framebuffer::new(
                framebuffer.address,
                framebuffer.width,
                framebuffer.height,
                framebuffer.stride,
                framebuffer.pixel_format,
            )
        };

        let bitmap_region = find_bitmap_region(memory_map);

        let layout = unsafe {
            KernelLayout {
                kernel_start: &__kernel_start as *const u8 as u64,
                kernel_end: &__kernel_end as *const u8 as u64,
                text_start: &__text_start as *const u8 as u64,
                text_end: &__text_end as *const u8 as u64,
                rela_dyn_start: &__rela_dyn_start as *const u8 as u64,
                rela_dyn_end: &__rela_dyn_end as *const u8 as u64,
                rodata_start: &__rodata_start as *const u8 as u64,
                rodata_end: &__rodata_end as *const u8 as u64,
            }
        };

        let mut allocator = unsafe {
            BitmapAllocator::new(
                bitmap_region,
                memory_map,
                layout.kernel_start,
                layout.kernel_end,
            )
        };

        // Reserve the kernel image so allocator won't hand out those frames
        allocator.reserve_region(layout.kernel_start, layout.kernel_end);

        unsafe { heap::init(&mut allocator) };

        // Initialise ACPI subsystem from the RSDP address.
        let acpi = if rsdp_addr != 0 {
            match AcpiSubsystem::new(rsdp_addr) {
                Ok(a) => {
                    log::info!("ACPI subsystem initialised");
                    Some(a)
                }
                Err(e) => {
                    log::warn!("ACPI init failed: {:?}", e);
                    None
                }
            }
        } else {
            log::info!("No RSDP address provided — ACPI disabled");
            None
        };

        Kernel {
            framebuffer: display,
            allocator,
            layout,
            stack_guard,
            memory_map,
            acpi,
        }
    }

    pub fn init(&mut self) {
        CurrentArch::init();
        CurrentArch::setup_virt_mem(
            &mut self.allocator,
            &self.layout,
            self.stack_guard,
            self.framebuffer.ptr() as u64,
            self.framebuffer.height(),
            self.framebuffer.stride(),
        );

        // Initialise AML interpreter (needs page tables live for MMIO).
        if let Some(ref mut acpi) = self.acpi {
            if let Err(e) = acpi.init_aml() {
                log::warn!("ACPI AML init failed: {:?}", e);
            }
        }

        // Enable interrupts after arch init and page tables are live.
        CurrentArch::enable_interrupts();
    }

    pub fn run(&mut self) -> ! {
        use display::Display;
        self.framebuffer.clear();
        init_all(&mut self.framebuffer);
        loop {
            CurrentArch::halt();
        }
    }
}

fn find_bitmap_region(memory_map: &[MemoryRegion]) -> (u64, u64) {
    let mut best = (0u64, 0u64);
    for region in memory_map {
        if region.kind == crate::boot::MemoryRegionKind::Usable
            && region.size > best.1
        {
            best = (region.base, region.size);
        }
    }
    assert!(best.1 > 0, "no usable memory region found in memory map");
    best
}
