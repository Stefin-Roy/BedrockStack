#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
extern crate alloc;

pub mod acpi;
pub mod acpi_log;
pub mod arch;
#[cfg(target_arch = "x86_64")]
pub mod audio;
pub mod boot;
#[cfg(all(target_arch = "x86_64", feature = "bootanim"))]
pub mod bootanim;
pub mod display;
pub mod drivers;
#[cfg(target_arch = "riscv64")]
pub mod dtb;
pub mod filesystems;
pub mod input;
#[cfg(target_arch = "x86_64")]
pub mod kerneldump;
pub mod bootargs;
pub mod mm;
pub mod pci;
pub mod platform;
pub mod random;
pub mod services;
pub mod smp;
#[cfg(target_arch = "x86_64")]
pub mod task;
pub mod unispace;
pub mod caps;
#[cfg(target_arch = "x86_64")]
pub mod iommu;
#[cfg(target_arch = "x86_64")]
pub mod usb;

use acpi::AcpiSubsystem;
use arch::CurrentArch;
use boot::{FramebufferInfo, MemoryRegion};
use framebuffer::Framebuffer;

use mm::heap;
use mm::phys_alloc::BitmapAllocator;
use mm::vmm;

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
    #[cfg(target_arch = "x86_64")]
    pub static __idt_start: u8;
    #[cfg(target_arch = "x86_64")]
    pub static __idt_end: u8;
    // Absolute linker symbols holding the kernel image's PHYSICAL (LMA)
    // bounds, defined in linker.ld for x86_64 (`__kernel_start_phys =
    // __low_end`, `__kernel_end_phys = __kernel_end + __phys_delta`, and
    // `__stack_start_phys = LOADADDR(.stack)`).  The RISC-V script does not
    // define these (that kernel is not yet higher-half), so they are gated.
    #[cfg(target_arch = "x86_64")]
    static __kernel_start_phys: u8;
    #[cfg(target_arch = "x86_64")]
    static __kernel_end_phys: u8;
    #[cfg(target_arch = "x86_64")]
    pub static __stack_start_phys: u8;
}

/// Higher-half VIRTUAL-address boundaries of the kernel image sections, used
/// to apply W^X permissions when building the page tables (the kernel links
/// at `KERNEL_VMA_BASE`).  Physical (LMA) bounds live in the `__*_phys`
/// linker symbols, which the physical allocator uses instead.
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
    #[cfg(target_arch = "x86_64")]
    pub idt_start: u64,
    #[cfg(target_arch = "x86_64")]
    pub idt_end: u64,
}

pub struct Kernel {
    framebuffer: Framebuffer,
    /// Physical address of the boot framebuffer, kept here for paging setup
    /// (which maps it identity).  Drivers access the framebuffer through the
    /// `Framebuffer` VA, never through this raw address.
    fb_phys: u64,
    allocator: BitmapAllocator,
    layout: KernelLayout,
    stack_guard: u64,
    rsdp_addr: u64,
    rsdp_data: Option<&'static [u8]>,
    acpi: Option<AcpiSubsystem>,
    page_table_root: u64,
    services: Option<&'static crate::services::KernelServices>,
}

impl Kernel {
    /// # Safety
    /// memory_map must be a valid slice of MemoryRegion.
    /// framebuffer must be a valid reference to data collected before exit_boot_services.
    /// stack_guard is the physical address of the stack guard page to leave unmapped.
    ///
    /// # Handoff-pointer lifetime (Phase 5)
    /// `memory_map` and `framebuffer` point into low PHYSICAL memory handed
    /// over by the bootloader.  `Kernel::new` runs BEFORE `switch_to_higher_half`,
    /// while the static `.boottables` identity map is still live, so the raw
    /// derefs below (bitmap sizing, `Framebuffer::new`) are safe.  Nothing
    /// after the switch may deref these raw pointers: the only values retained
    /// are `fb_phys` (covered by the identity framebuffer window that
    /// `paging::setup` maps) and `rsdp_addr` (deref'd through the VMM), and
    /// `rsdp_data` is either absent (UEFI path) or a `'static` copy that was
    /// moved into kernel memory before the switch (MB2 path).
    pub unsafe fn new(
        memory_map: &'static [MemoryRegion],
        framebuffer: &FramebufferInfo,
        stack_guard: u64,
        rsdp_addr: u64,
        rsdp_data: Option<&'static [u8]>,
    ) -> Self {
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[kernel] Kernel::new: acpi_log init\n");
        crate::acpi_log::init();

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
                #[cfg(target_arch = "x86_64")]
                idt_start: &__idt_start as *const u8 as u64,
                #[cfg(target_arch = "x86_64")]
                idt_end: &__idt_end as *const u8 as u64,
            }
        };

        SerialPort::puts("[kernel] Kernel::new: BitmapAllocator::new\n");
        // The allocator consumes PHYSICAL kernel bounds (bitmap placement,
        // frame reservation).  `layout.*` holds higher-half VMAs for paging
        // (Phase 4), so the physical extent comes from the `__*_phys` linker
        // symbols.  RISC-V still links low (VMA == physical), so its VMA
        // bounds ARE the physical bounds.
        #[cfg(target_arch = "x86_64")]
        let (kernel_start_phys, kernel_end_phys) = unsafe {
            (
                &__kernel_start_phys as *const u8 as u64,
                &__kernel_end_phys as *const u8 as u64,
            )
        };
        #[cfg(target_arch = "riscv64")]
        let (kernel_start_phys, kernel_end_phys) = (layout.kernel_start, layout.kernel_end);
        let mut allocator = unsafe {
            BitmapAllocator::new(
                bitmap_region,
                memory_map,
                kernel_start_phys,
                kernel_end_phys,
            )
        };

        SerialPort::puts("[kernel] Kernel::new: reserve_region\n");
        allocator.reserve_region(kernel_start_phys, kernel_end_phys);

        SerialPort::puts("[kernel] Kernel::new: framebuffer\n");
        let fb_size = match (framebuffer.stride as u64)
            .checked_mul(framebuffer.height as u64)
            .and_then(|v| v.checked_mul(framebuffer.bpp as u64))
        {
            Some(s) => s as usize,
            None => {
                SerialPort::puts("[kernel] WARN: framebuffer size overflow, no fb\n");
                0
            }
        };
        // Belt-and-suspenders: reserve the framebuffer's physical range in the
        // allocator so we never hand out the GPU's own memory as system RAM.
        allocator.reserve_range(framebuffer.address, fb_size as u64);
        // Phase D: no physical shadow buffer here.  The shadow is a heap/
        // guard-mapped VM-backed allocation that is created in `init()` once
        // the heap is live, and bound via `set_shadow_va()`.  Until then the
        // framebuffer's shadow pointer is null (every accessor already treats a
        // null shadow as inert).
        let display = unsafe {
            Framebuffer::new(
                framebuffer.address,
                framebuffer.width,
                framebuffer.height,
                framebuffer.stride,
                framebuffer.pixel_format,
                framebuffer.bpp,
                0,
            )
        };

        SerialPort::puts("[kernel] Kernel::new: done\n");

        Kernel {
            framebuffer: display,
            fb_phys: framebuffer.address,
            allocator,
            layout,
            stack_guard,
            rsdp_addr,
            rsdp_data,
            acpi: None,
            page_table_root: 0,
            services: None,
        }
    }

    /// Access the service container (panics if not yet initialised).
    fn svc(&self) -> &crate::services::KernelServices {
        self.services
            .as_ref()
            .expect("KernelServices not initialised")
    }

    pub fn init(&mut self) {
        // The physical allocator was moved during `Kernel::new()`; re-point
        // the stashed heap/DMA pointer before any code path can need it.
        heap::set_phys_allocator(&mut self.allocator);
        unsafe {
            crate::smp::early_init_bsp();
        }
        // Early CSPRNG — RDRAND + TSC jitter before paging, no heap/RTC.
        crate::random::init_early();
        // Real KASLR: 4 MiB Csprng, filtered, actually slid in paging::setup.
        crate::mm::layout::init_kaslr();
        self.switch_to_higher_half();
        crate::mm::layout::verify_layout();

        // The heap lives in a mapped arena above KERNEL_VMA_BASE, so it can be
        // initialised only once the kernel page tables are live. Nothing
        // between `new()` and here allocates from the heap, so moving it after
        // `switch_to_higher_half` is safe.
        unsafe {
            heap::init(self.page_table_root, &mut self.allocator);
        }
        crate::drivers::serial::switch_to_growable();

        CurrentArch::init();

        // Strong reseed — mixes calibrated TSC + RTC seconds.
        crate::random::reseed_strong();

        // Parse ACPI tables (needs VMM live for mapped physical regions).
        self.init_acpi();

        // Reserve DMAR RMRR ranges early (before the service container hands
        // out frames). RMRR is BIOS-reserved memory that must stay identity-
        // mapped for legacy devices (USB, graphics). Even with `noiommu` we
        // must not reallocate it — the device may DMA there untranslated.
        #[cfg(target_arch = "x86_64")]
        if let Some(ref acpi) = self.acpi {
            if let Some(ref dmar) = acpi.dmar {
                for rmrr in &dmar.rmrrs {
                    let start = rmrr.base_address & !0xFFF;
                    let end = (rmrr.limit_address | 0xFFF) + 1; // exclusive
                    if end > start {
                        crate::drivers::serial::SerialPort::puts("[init] RMRR reserve ");
                        crate::drivers::serial::SerialPort::put_hex(start);
                        crate::drivers::serial::SerialPort::puts(" - ");
                        crate::drivers::serial::SerialPort::put_hex(end);
                        crate::drivers::serial::SerialPort::puts("\n");
                        self.allocator.reserve_range(start, end - start);
                    }
                }
            }
        }

        // Initialise I/O APIC(s) from ACPI interrupt model (x86_64 only).
        #[cfg(target_arch = "x86_64")]
        self.init_ioapic();

        // Build the service container for driver dispatch.
        let acpi_static = self
            .acpi
            .as_ref()
            .map(|a| unsafe { &*(a as *const crate::acpi::AcpiSubsystem) });
        let svc = alloc::boxed::Box::new(crate::services::init_services(
            self.page_table_root,
            &mut self.allocator as *mut _,
            acpi_static,
        ));
        let svc_static: &'static crate::services::KernelServices = alloc::boxed::Box::leak(svc);
        crate::services::set_global(svc_static);
        self.services = Some(svc_static);

        // Phase D: bind the framebuffer's shadow buffer to a heap (guard-mapped,
        // NX) VM-backed allocation via direct kernel-heap alloc. Nothing
        // dereferences the display until `run()`.
        self.init_framebuffer_shadow();

        // Initialise SMP — discover and start Application Processors.
        let ncpus =
            unsafe { crate::smp::init(self.page_table_root, self.acpi.as_ref(), self.svc()) };
        log::info!("SMP: {} CPU(s) online", ncpus);
        crate::drivers::serial::SerialPort::puts("[init] SMP done, enabling interrupts\n");

        // Enable interrupts after arch init, page tables, and SMP are live.
        self.svc().platform.enable_interrupts();
    }

    /// Phase D: allocate the framebuffer shadow buffer on the kernel heap and bind it.
    ///
    /// The shadow lives in the guard-mapped, NX heap arena rather than as raw
    /// contiguous physical frames, so the display path never dereferences a
    /// physical address. The allocation is deliberately leaked: it is needed
    /// for the lifetime of the kernel and `Framebuffer` keeps no ownership.
    fn init_framebuffer_shadow(&mut self) {
        let size = self.framebuffer.total_bytes();
        log::info!(
            "framebuffer: phys=0x{:x} {}x{} stride={} bpp={} ({} B shadow via heap)",
            self.fb_phys,
            self.framebuffer.width(),
            self.framebuffer.height(),
            self.framebuffer.stride(),
            self.framebuffer.bpp(),
            size
        );
        // Direct kernel-heap allocation: the global allocator (set up in
        // `heap::init` during `init()`) serves this from the guard-mapped,
        // NX heap arena — exactly what `Framebuffer::set_shadow_va` expects.
        let mut shadow: alloc::vec::Vec<u8> = alloc::vec![0u8; size];
        let va = shadow.as_mut_ptr() as u64;
        core::mem::forget(shadow);
        self.framebuffer.set_shadow_va(va);

        // Point the framebuffer at the higher-half FB window (x86_64) and
        // snapshot it for the `/dev/fb` device; riscv64 keeps the identity VA.
        #[cfg(target_arch = "x86_64")]
        self.framebuffer.set_fb_va(crate::mm::layout::FB_VADDR_BASE);
        crate::display::register(&self.framebuffer);
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
            self.fb_phys,
            self.framebuffer.height(),
            self.framebuffer.stride(),
            self.framebuffer.bpp(),
        );
        let root = vmm.root();
        unsafe {
            vmm::activate(root);
            // DIRECT_MAP was built into these tables by paging::setup; from
            // here on the VMM walkers deref page-table frames through the
            // kernel-internal physmap instead of the identity map.
            crate::mm::init_physmap(self.allocator.alloc_end());
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
        if self.rsdp_addr == 0 && self.rsdp_data.is_none() {
            log::info!("No RSDP address provided -- ACPI disabled");
            return;
        }
        match AcpiSubsystem::new(self.rsdp_addr, self.rsdp_data) {
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
        // The physical allocator was moved from the stack of `new()` into
        // `self.allocator`, leaving the raw pointer stashed by `heap::init`
        // dangling.  Re-point it at the final (stable) address.
        heap::set_phys_allocator(&mut self.allocator);

        // Lock down the IDT — .idt section becomes read-only.
        // Any wild write through the IDT's page will now page-fault immediately.
        #[cfg(target_arch = "x86_64")]
        crate::arch::x86_64::idt::protect_idt(
            self.page_table_root,
            self.layout.idt_start,
            self.layout.idt_end,
        );

        // Boot animation: start the indeterminate spinner, which ticks on the
        // universal timer for the whole of the boot tail below.
        #[cfg(all(target_arch = "x86_64", feature = "bootanim"))]
        crate::bootanim::start(&mut self.framebuffer);

        // Initialise PCI subsystem (ECAM mapping + bus enumeration).
        if let Some(ref acpi) = self.acpi {
            crate::pci::init(
                &acpi.pci_config_regions,
                self.page_table_root,
                &mut self.allocator as *mut _,
            );
        }

        // UInputL — the unified input layer.  Must exist before any driver
        // (PS/2, future USB HID) tries to register a device.
        crate::input::init();

        // PS/2 keyboard driver (8042 controller) — registers IRQ 1 and the
        // keyboard device for the unified input layer.
        #[cfg(target_arch = "x86_64")]
        crate::drivers::ps2::init();

        #[cfg(target_arch = "x86_64")]
        {
            crate::drivers::serial::SerialPort::puts("\n=== vec34=");
            crate::drivers::serial::SerialPort::put_u64(crate::arch::x86_64::idt::vec34_count());
            crate::drivers::serial::SerialPort::puts(" xhci_irq=");
            crate::drivers::serial::SerialPort::put_u64(crate::usb::xhci::event::irq_count());
            crate::drivers::serial::SerialPort::puts(" ===\n");
        }

        #[cfg(target_arch = "x86_64")]
        let mut block_devices =
            crate::filesystems::blockdriver::driver::init_all(crate::pci::devices());

        #[cfg(target_arch = "x86_64")]
        {
            crate::drivers::serial::SerialPort::puts("\n=== vec34=");
            crate::drivers::serial::SerialPort::put_u64(crate::arch::x86_64::idt::vec34_count());
            crate::drivers::serial::SerialPort::puts(" xhci_irq=");
            crate::drivers::serial::SerialPort::put_u64(crate::usb::xhci::event::irq_count());
            crate::drivers::serial::SerialPort::puts(" ===\n");
        }

        #[cfg(target_arch = "x86_64")]
        let usb_block_devices = crate::usb::xhci::init_all(crate::pci::devices());

        // Audio subsystem — probes PCI for an Intel HD Audio controller.
        #[cfg(target_arch = "x86_64")]
        crate::audio::init();

        // Capture self-test: with a duplex codec, record ~250 ms through the
        // feeding input ring and report what came back.  All-zero audio (no
        // host mic configured for the QEMU backend) still proves the input
        // DMA moved — BCIS fired and the ring advanced.  Gated behind
        // `selftest` so routine boots are untouched.
        #[cfg(target_arch = "x86_64")]
        #[cfg(feature = "selftest")]
        {
            use crate::drivers::serial::SerialPort as SP;
            if crate::audio::can_record() {
                let total_bytes = 48_000u32; // 250 ms of stereo 16-bit @ 48 kHz
                let mut recorded: u64 = 0;
                let mut peak: u32 = 0;
                let mut rms_acc: u64 = 0;
                let mut n_sum: u64 = 0;
                let mut chunk: alloc::vec::Vec<i16> = alloc::vec![0i16; 4096];
                let r = loop {
                    match crate::audio::record_pcm(&mut chunk) {
                        Ok(()) => {}
                        Err(e) => break Err(e),
                    }
                    recorded += (chunk.len() * 2) as u64;
                    for s in &chunk {
                        let a = s.unsigned_abs() as u64;
                        rms_acc += (a * a) >> 8;
                        n_sum += 1;
                        peak = peak.max(a as u32);
                    }
                    if recorded >= total_bytes as u64 {
                        break Ok(recorded);
                    }
                };
                SP::puts("[audio] selftest capture: ");
                match r {
                    Ok(b) => {
                        SP::puts("ok bytes=");
                        SP::put_u64(b);
                    }
                    Err(e) => {
                        SP::puts("err ");
                        SP::puts(e);
                    }
                }
                SP::puts(" total=");
                SP::put_u64(recorded);
                SP::puts(" peak=");
                SP::put_hex(peak as u64);
                if n_sum > 0 {
                    SP::puts(" rms8=");
                    SP::put_hex(rms_acc / n_sum);
                }
                SP::puts("\n");
            }
        }

        #[cfg(target_arch = "x86_64")]
        {
            crate::drivers::serial::SerialPort::puts("\n=== vec34=");
            crate::drivers::serial::SerialPort::put_u64(crate::arch::x86_64::idt::vec34_count());
            crate::drivers::serial::SerialPort::puts(" xhci_irq=");
            crate::drivers::serial::SerialPort::put_u64(crate::usb::xhci::event::irq_count());
            crate::drivers::serial::SerialPort::puts(" ===\n");
        }

        #[cfg(target_arch = "x86_64")]
        {
            block_devices.extend(usb_block_devices);
            *crate::filesystems::blockdriver::driver::BLOCK_DEVICES.lock() = block_devices.clone();
        }

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

        // Read-only NTFS demo drive (second block device) as C>, exercised
        // only under `selftest` so routine boots are untouched.
        #[cfg(all(target_arch = "x86_64", feature = "selftest"))]
        if let Some(dev) = block_devices.get(1) {
            match crate::filesystems::partition::mount_first_partition(dev.clone(), "ntfs", 'C') {
                Ok(()) => {
                    log::info!("Mounted NTFS demo as C> (read-only)");
                    crate::filesystems::fstypes::ntfs::selftest::run();
                }
                Err(e) => {
                    let probe = crate::filesystems::partition::last_mount_detail().unwrap_or("none");
                    let ntfs = crate::filesystems::fstypes::ntfs::last_error().unwrap_or("none");
                    // Discriminant helps without parsing Debug; probe/ntfs pinpoint the
                    // collapsed IOError: "sector read error" vs "usa_fixup" vs "MFT FILE miss".
                    log::warn!(
                        "Could not mount NTFS demo on C>: {:?} discriminant={}({}) probe_detail={:?} ntfs_detail={:?} model={:?} sectors={}",
                        e,
                        e.discriminant_name(),
                        e.discriminant_value(),
                        probe,
                        ntfs,
                        dev.model_string(),
                        dev.sector_count()
                    );
                    // Always mirror to raw serial for boots where the logger level
                    // filters WARN, and so the triple is visible even without `log`.
                    crate::drivers::serial::SerialPort::puts("[ntfs] mount C> failed: VfsError::");
                    crate::drivers::serial::SerialPort::puts(e.discriminant_name());
                    crate::drivers::serial::SerialPort::puts(" probe='");
                    crate::drivers::serial::SerialPort::puts(probe);
                    crate::drivers::serial::SerialPort::puts("' ntfs='");
                    crate::drivers::serial::SerialPort::puts(ntfs);
                    crate::drivers::serial::SerialPort::puts("' dev='");
                    crate::drivers::serial::SerialPort::puts(dev.model_string());
                    crate::drivers::serial::SerialPort::puts("'\n");
                }
            }
        } else {
            #[cfg(all(target_arch = "x86_64", feature = "selftest"))]
            {
                let count = block_devices.len();
                log::warn!("NTFS demo not mounted: second block device missing (block_devices={})", count);
                crate::drivers::serial::SerialPort::puts("[ntfs] mount C> skipped: second block device missing, block_devices=");
                crate::drivers::serial::SerialPort::put_u64(count as u64);
                crate::drivers::serial::SerialPort::puts("\n");
                if count > 0 {
                    crate::drivers::serial::SerialPort::puts("[ntfs] dev0 model='");
                    crate::drivers::serial::SerialPort::puts(block_devices[0].model_string());
                    crate::drivers::serial::SerialPort::puts("'\n");
                }
            }
        }

        // Unispace: build the / registry, attach the providers (VFS mounts,
        // /sys), then run the boot self-test (gated behind `selftest`).
        crate::unispace::init();
        match crate::unispace::provider::register_all() {
            Ok(()) => log::info!("unispace: providers registered"),
            Err(e) => log::warn!("unispace: provider registration failed: {:?}", e),
        }
        #[cfg(feature = "selftest")]
        crate::unispace::self_test();

        // Cooperative scheduler init (needed for the INIT launch below).
        #[cfg(target_arch = "x86_64")]
        {
            crate::task::init(self.page_table_root);
            #[cfg(feature = "selftest")]
            crate::task::smoke_test(&mut self.allocator);
            // Audio pump: fire-forget queue so play_pcm/play_tone return
            // immediately and never block INIT/DOOM; ISR zeroes consumed slots
            // so drained audio becomes silence — no stale repetition after
            // provider finished (old CBL=u32::MAX pump). 16×1024 ring
            // (≈85 ms, staged cap ≈80 ms) + queue 4 for latency/robustness.
            crate::audio::spawn_pump(&mut self.allocator);
        }

        // Stop the spinner: INIT's desktop paint takes over the screen now.
        #[cfg(all(target_arch = "x86_64", feature = "bootanim"))]
        crate::bootanim::stop();

        // Phase 6: load \EFI\BEDROCK\INIT from the ESP (via the unispace /B
        // mount) into its own address space and drop to ring 3. No-ops with a
        // serial notice when INIT is absent. Control returns only after INIT
        // has exited and parked into idle; the poll/halt loop below is the
        // long-lived idle from then on.
        #[cfg(target_arch = "x86_64")]
        crate::task::load::load_init_from_esp(&mut self.allocator);

        loop {
            #[cfg(target_arch = "x86_64")]
            {
                // IOMMU fault poll (backup for the fault MSI at vector 53).
                // The IRQ drains the FRCD queue; polling catches faults even
                // if the MSI was not delivered (e.g., masked or coalesced).
                // Non-halting: faults are logged and ignored.
                #[cfg(target_arch = "x86_64")]
                if crate::iommu::is_enabled() {
                    crate::iommu::fault_handler();
                }

                // Reap parked dead tasks: free their user page tables, kernel
                // stacks, and task boxes.  Runs here (idle stack, kernel CR3)
                // so no task is ever torn down while parked on its own stack.
                crate::task::reap_dead(&mut self.allocator);

                // Hot-plug: poll the retained xHCI controller for port
                // changes and register any newly attached block devices.
                let new_devices = crate::usb::xhci::poll();
                if !new_devices.is_empty() {
                    crate::filesystems::blockdriver::driver::BLOCK_DEVICES
                        .lock()
                        .extend(new_devices);
                }

                // Run any ready task, including sleepers whose deadline has
                // passed (wake_sleepers moves them back to Ready first).
                // Returns to idle once every task is running, sleeping, or
                // dead — the timer ISR never touches the scheduler, so all
                // sleep bookkeeping happens here in idle context.
                crate::task::wake_sleepers();
                crate::task::schedule();

                // Nothing ready: park until the earliest sleeping deadline so
                // a sleeper wakes on time, else fall through to the plain
                // device-IRQ halt below.
                if let Some(d) = crate::task::earliest_sleep_deadline() {
                    crate::services::universal_timer::wait_until(d.saturating_add(1));
                    continue;
                }
            }
            self.svc().platform.halt();
        }
    }
}

fn find_bitmap_region(memory_map: &[MemoryRegion]) -> (u64, u64) {
    // Prefer the largest usable region below 4 GiB, which is guaranteed
    // to be identity-mapped by both GRUB's initial 1 GiB map and UEFI's
    // page tables.  The bitmap (~300 KiB for 32 GiB RAM) fits easily.
    let mut best = (0u64, 0u64);
    for region in memory_map {
        if region.kind == crate::boot::MemoryRegionKind::Usable
            && region.base < 0x100000000
            && region.size > best.1
        {
            best = (region.base, region.size);
        }
    }
    if best.1 > 0 {
        return best;
    }

    // Fall back to the largest usable region overall (may be above 4 GiB
    // on systems with no low-memory RAM, e.g. certain NUMA configs).
    for region in memory_map {
        if region.kind == crate::boot::MemoryRegionKind::Usable && region.size > best.1 {
            best = (region.base, region.size);
        }
    }
    assert!(best.1 > 0, "no usable memory region found in memory map");
    best
}
