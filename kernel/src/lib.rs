#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
extern crate alloc;

pub mod acpi;
pub mod arch;
pub mod boot;
pub mod display;
pub mod drivers;
pub mod filesystems;
#[cfg(target_arch = "riscv64")]
pub mod dtb;
pub mod acpi_log;
#[cfg(target_arch = "x86_64")]
pub mod kerneldump;
pub mod mm;
pub mod module;
pub mod pci;
pub mod platform;
pub mod smp;

use acpi::AcpiSubsystem;
use arch::{Arch, CurrentArch};
use boot::{FramebufferInfo, MemoryRegion};
use display::framebuffer::Framebuffer;

use mm::heap;
use mm::phys_alloc::BitmapAllocator;
use mm::vmm;
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
    pub static __stack_start: u8;
    pub static __stack_end: u8;
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
    rsdp_addr: u64,
    acpi: Option<AcpiSubsystem>,
    page_table_root: u64,
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
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[kernel] Kernel::new: acpi_log init\n");
        crate::acpi_log::init();

        SerialPort::puts("[kernel] Kernel::new: framebuffer\n");
        let display = unsafe {
            Framebuffer::new(
                framebuffer.address,
                framebuffer.width,
                framebuffer.height,
                framebuffer.stride,
                framebuffer.pixel_format,
            )
        };

        #[cfg(feature = "display_log")]
        {
            use crate::display::console::Console;
            let console = unsafe {
                Console::new(
                    display.ptr(),
                    display.width(),
                    display.height(),
                    display.stride(),
                    display.pixel_format(),
                )
            };
            crate::drivers::serial::set_console(console);
        }

        SerialPort::puts("[kernel] Kernel::new: find_bitmap_region\n");
        let bitmap_region = find_bitmap_region(memory_map);

        SerialPort::puts("[kernel] Kernel::new: layout\n");
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

        SerialPort::puts("[kernel] Kernel::new: BitmapAllocator::new\n");
        let mut allocator = unsafe {
            BitmapAllocator::new(
                bitmap_region,
                memory_map,
                layout.kernel_start,
                layout.kernel_end,
            )
        };

        SerialPort::puts("[kernel] Kernel::new: reserve_region\n");
        allocator.reserve_region(layout.kernel_start, layout.kernel_end);

        SerialPort::puts("[kernel] Kernel::new: heap::init\n");
        unsafe { heap::init(&mut allocator) };
        SerialPort::puts("[kernel] Kernel::new: done\n");

        Kernel {
            framebuffer: display,
            allocator,
            layout,
            stack_guard,
            rsdp_addr,
            acpi: None,
            page_table_root: 0,
        }
    }

    pub fn init(&mut self) {
        // The physical allocator was moved during `Kernel::new()`; re-point
        // the stashed heap/DMA pointer before any code path can need it.
        heap::set_phys_allocator(&mut self.allocator);
        unsafe { crate::smp::early_init_bsp(); }
        CurrentArch::init();
        self.switch_to_higher_half();

        // Parse ACPI tables (needs VMM live for mapped physical regions).
        self.init_acpi();

        // NOTE: AML interpreter init (DSDT/SSDT parse) hangs on QEMU;
        // AML is only used for SLP_TYP detection on shutdown, and the
        // default (0x00) works fine on virtual hardware — skip for now.
        // if let Some(ref mut acpi) = self.acpi {
        //     if let Err(e) = acpi.init_aml() {
        //         log::warn!("ACPI AML init failed: {:?}", e);
        //     }
        // }

        // Initialise I/O APIC(s) from ACPI interrupt model (x86_64 only).
        #[cfg(target_arch = "x86_64")]
        self.init_ioapic();

        // Initialise SMP — discover and start Application Processors.
        let ncpus = unsafe {
            crate::smp::init(&mut self.allocator, self.page_table_root, self.acpi.as_ref())
        };
        log::info!("SMP: {} CPU(s) online", ncpus);
        crate::drivers::serial::SerialPort::puts("[init] SMP done, enabling interrupts\n");

        // Enable interrupts after arch init, page tables, and SMP are live.
        CurrentArch::enable_interrupts();
    }

    /// Parse the ACPI interrupt model and initialise I/O APIC(s).
    #[cfg(target_arch = "x86_64")]
    fn init_ioapic(&mut self) {
        let acpi = match self.acpi.as_ref() {
            Some(a) => a,
            None => return,
        };
        if let crate::acpi::InterruptModel::Apic(apic) = &acpi.interrupt_model {
            for io_apic in &apic.io_apics {
                crate::platform::x86_64_pc::ioapic::init(
                    io_apic.address as u64,
                    io_apic.global_system_interrupt_base,
                );
            }
        }
    }

    /// Build page tables with identity maps + a higher-half kernel alias,
    /// then activate them (switch CR3 / SATP).
    fn switch_to_higher_half(&mut self) {
        let vmm = CurrentArch::setup_virt_mem(
            &mut self.allocator,
            &self.layout,
            self.stack_guard,
            self.framebuffer.ptr() as u64,
            self.framebuffer.height(),
            self.framebuffer.stride(),
        );
        let root = vmm.root();
        unsafe {
            vmm::activate(root);
            crate::acpi::init_vmm(root, &mut self.allocator as *mut _);
        }
        self.page_table_root = root;
        log::info!("Higher-half page tables activated");
    }

    /// Parse ACPI tables from the RSDP.
    ///
    /// Runs after page tables are live so the VMM-backed `AcpiHandler` can
    /// map physical regions.
    fn init_acpi(&mut self) {
        if self.rsdp_addr == 0 {
            log::info!("No RSDP address provided — ACPI disabled");
            return;
        }
        match AcpiSubsystem::new(self.rsdp_addr) {
            Ok(a) => {
                log::info!("ACPI subsystem initialised");
                self.acpi = Some(a);
            }
            Err(e) => {
                log::warn!("ACPI init failed: {:?}", e);
            }
        }
    }

    pub fn run(&mut self) -> ! {
        use display::Display;
        self.framebuffer.clear();

        // The physical allocator was moved from the stack of `new()` into
        // `self.allocator`, leaving the raw pointer stashed by `heap::init`
        // dangling.  Re-point it at the final (stable) address.
        heap::set_phys_allocator(&mut self.allocator);

        // Initialise PCI subsystem (ECAM mapping + bus enumeration).
        if let Some(ref acpi) = self.acpi {
            crate::pci::init(
                &acpi.pci_config_regions,
                self.page_table_root,
                &mut self.allocator as *mut _,
            );
        }

        #[cfg(target_arch = "x86_64")]
        let block_devices = crate::filesystems::blockdriver::driver::init_all(
            crate::pci::devices(),
            self.page_table_root,
            &mut self.allocator as *mut _,
        );

        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        crate::filesystems::vfs::init().expect("VFS init failed");

        // Mount the ESP (first partition on first block device) as B> (fat32)
        #[cfg(target_arch = "x86_64")]
        if let Some(dev) = block_devices.first() {
            match crate::filesystems::partition::mount_first_partition(dev.clone(), "fat32", 'B') {
                Ok(()) => log::info!("Mounted ESP as B> (fat32)"),
                Err(e) => log::warn!("Could not mount ESP on B>: {:?}", e),
            }
        }

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
