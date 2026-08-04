#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
extern crate alloc;

pub mod acpi;
pub mod arch;
#[cfg(target_arch = "x86_64")]
pub mod audio;
pub mod boot;
pub mod drivers;
pub mod filesystems;
#[cfg(target_arch = "riscv64")]
pub mod dtb;
pub mod acpi_log;
pub mod input;
#[cfg(target_arch = "x86_64")]
pub mod kerneldump;
pub mod mm;
pub mod obj;
pub mod pci;
pub mod platform;
#[cfg(target_arch = "x86_64")]
pub mod usb;
pub mod smp;
pub mod services;

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
    pub static __idt_start: u8;
    pub static __idt_end: u8;
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
    pub idt_start: u64,
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
                idt_start: &__idt_start as *const u8 as u64,
                idt_end: &__idt_end as *const u8 as u64,
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

        SerialPort::puts("[kernel] Kernel::new: framebuffer\n");
        let fb_size = (framebuffer.stride as u64)
            .checked_mul(framebuffer.height as u64)
            .and_then(|v| v.checked_mul(framebuffer.bpp as u64))
            .expect("framebuffer size overflow") as usize;
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
        self.services.as_ref().expect("KernelServices not initialised")
    }

    pub fn init(&mut self) {
        // The physical allocator was moved during `Kernel::new()`; re-point
        // the stashed heap/DMA pointer before any code path can need it.
        heap::set_phys_allocator(&mut self.allocator);
        unsafe { crate::smp::early_init_bsp(); }
        self.switch_to_higher_half();
        crate::mm::layout::verify_layout();

        // The heap lives in a mapped arena above KERNEL_VMA_BASE, so it can be
        // initialised only once the kernel page tables are live. Nothing
        // between `new()` and here allocates from the heap, so moving it after
        // `switch_to_higher_half` is safe.
        unsafe {
            heap::init(self.page_table_root, &mut self.allocator);
        }

        CurrentArch::init();

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

        // Build the service container for capability-based dispatch.
        let acpi_static = self.acpi.as_ref().map(|a| unsafe { &*(a as *const crate::acpi::AcpiSubsystem) });
        let svc = alloc::boxed::Box::new(crate::services::init_services(
            self.page_table_root,
            &mut self.allocator as *mut _,
            acpi_static,
        ));
        let svc_static: &'static crate::services::KernelServices = alloc::boxed::Box::leak(svc);
        self.services = Some(svc_static);

        // C5 — RootGraph bootstrap: create the Boot domain, mint the primitive
        // family roots, and endow the real service providers as capabilities
        // reachable only through the boot table (§5.4).
        crate::obj::bootstrap::bootstrap(self.page_table_root, svc_static);
        crate::drivers::serial::SerialPort::puts("[obj] bootstrap: boot domain endowed\n");
        crate::obj::domain::register_domain(crate::obj::bootstrap::boot_domain());
        crate::obj::domain::register_domain(crate::obj::driver::driver_domain());

        // C6 — boot-time separation proof: the endowed DMA capability works,
        // unendowed ids and foreign contracts are refused. Runs once, before SMP.
        crate::obj::separation::run();
        crate::obj::paged_isolation::run();

        // Phase D: bind the framebuffer's shadow buffer to a heap (guard-mapped,
        // NX) VM-backed allocation. Runs AFTER bootstrap so the allocation is
        // routed through the Boot domain's Heap family-root capability (§7.10.2)
        // instead of a raw kernel-heap call. Nothing dereferences the display
        // until `run()`.
        self.init_framebuffer_shadow();

        // Initialise SMP — discover and start Application Processors.
        let ncpus = unsafe {
            crate::smp::init(self.page_table_root, self.acpi.as_ref(), self.svc())
        };
        log::info!("SMP: {} CPU(s) online", ncpus);
        crate::drivers::serial::SerialPort::puts("[init] SMP done, enabling interrupts\n");

        // Enable interrupts after arch init, page tables, and SMP are live.
        self.svc().platform.enable_interrupts();

        // C5 — bootstrapper self-revoke (last statement of init; §5.5, §8.2).
        // Mint authority returns to the Principal; the boot domain stays.
        crate::obj::mint::finalize_mint();
        crate::drivers::serial::SerialPort::puts("[obj] bootstrap self-revoke: mint authority returned to Principal\n");
    }

    /// Phase D: allocate the framebuffer shadow buffer on the heap and bind it.
    ///
    /// The shadow lives in the guard-mapped, NX heap arena rather than as raw
    /// contiguous physical frames, so the display path never dereferences a
    /// physical address. The allocation is deliberately leaked: it is needed
    /// for the lifetime of the kernel and `Framebuffer` keeps no ownership.
    ///
    /// The allocation is routed through the Boot domain's Heap family root
    /// (§7.10.2): invoke `heap:alloc`, recover the `mem:region` capability it
    /// replies, and read its base. The block is a kernel-heap `MemRegion`, so
    /// its base is already the virtual address of the guard-mapped arena —
    /// exactly what `Framebuffer::set_shadow_va` expects. If the cap-mediated
    /// path ever fails (e.g. pool exhaustion), we fall back to a plain kernel
    /// allocation so display stays available; both leaks are deliberate.
    fn init_framebuffer_shadow(&mut self) {
        use crate::obj::bootstrap::{boot_domain, boot_endowment};
        use crate::obj::memregion::{MEM_REGION_BASE, MEM_REGION_CONTRACT};
        use crate::obj::nodes::{HEAP_ALLOC, HEAP_CONTRACT};
        use crate::obj::{Args, Reply, Value, invoke};

        let size = self.framebuffer.total_bytes();
        let align = 8u64;
        let args = Args { vals: alloc::vec![Value::U64(size as u64), Value::U64(align)] };
        let table = &boot_domain().table;
        let va = match invoke(table, boot_endowment().heap, HEAP_CONTRACT, HEAP_ALLOC, &args) {
            Ok(Reply::Caps(caps)) if caps.len() == 1 => {
                let region_id = caps[0].id;
                match invoke(table, region_id, MEM_REGION_CONTRACT, MEM_REGION_BASE, &Args::none()) {
                    Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
                        Value::U64(base) => {
                            // Preserve the old behaviour: the shadow starts
                            // zeroed so the first un-drawn frame is black, not
                            // stale heap.
                            unsafe { core::ptr::write_bytes(*base as *mut u8, 0, size) };
                            log::info!("framebuffer shadow: {size} B via heap cap @ {base:#x}");
                            Some(*base)
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => None,
        };
        let va = match va {
            Some(va) => va,
            None => {
                log::warn!("framebuffer shadow: heap-cap path failed; falling back");
                let mut shadow: alloc::vec::Vec<u8> = alloc::vec![0u8; size];
                let va = shadow.as_mut_ptr() as u64;
                core::mem::forget(shadow);
                va
            }
        };
        self.framebuffer.set_shadow_va(va);
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
            log::info!("No RSDP address provided — ACPI disabled");
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

        // C8 — the device sweep runs as the first driver domain (§6.2): switch
        // the BSP's current-domain slot to the driver domain before PCI
        // enumeration. All device-path clients (driver_dma / driver_pci) are
        // bound to the driver domain's disjoint table, so the sweep genuinely
        // runs under its endowed caps only (§8.14).
        crate::obj::domain::set_current_domain(crate::obj::driver::driver_domain());

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
        // (PS/2, future USB HID) tries to register a device.
        crate::input::init();

        // PS/2 keyboard driver (8042 controller) — registers IRQ 1 and the
        // keyboard device for the unified input layer.
        #[cfg(target_arch = "x86_64")]
        crate::drivers::ps2::init();

        #[cfg(target_arch = "x86_64")]
        {
            crate::drivers::serial::SerialPort::puts("\n=== vec34=");
            crate::drivers::serial::SerialPort::put_u64(
                crate::arch::x86_64::idt::vec34_count());
            crate::drivers::serial::SerialPort::puts(" xhci_irq=");
            crate::drivers::serial::SerialPort::put_u64(
                crate::usb::xhci::event::irq_count());
            crate::drivers::serial::SerialPort::puts(" ===\n");
        }

        #[cfg(target_arch = "x86_64")]
        let mut block_devices = crate::filesystems::blockdriver::driver::init_all(
            crate::pci::devices(),
        );

        #[cfg(target_arch = "x86_64")]
        {
            crate::drivers::serial::SerialPort::puts("\n=== vec34=");
            crate::drivers::serial::SerialPort::put_u64(
                crate::arch::x86_64::idt::vec34_count());
            crate::drivers::serial::SerialPort::puts(" xhci_irq=");
            crate::drivers::serial::SerialPort::put_u64(
                crate::usb::xhci::event::irq_count());
            crate::drivers::serial::SerialPort::puts(" ===\n");
        }

        #[cfg(target_arch = "x86_64")]
        let usb_block_devices = crate::usb::xhci::init_all(
            crate::pci::devices(),
        );

        // Audio subsystem — probes PCI for an Intel HD Audio controller.
        #[cfg(target_arch = "x86_64")]
        crate::audio::init();
        crate::obj::devices::init_audio();

        #[cfg(target_arch = "x86_64")]
        {
            crate::drivers::serial::SerialPort::puts("\n=== vec34=");
            crate::drivers::serial::SerialPort::put_u64(
                crate::arch::x86_64::idt::vec34_count());
            crate::drivers::serial::SerialPort::puts(" xhci_irq=");
            crate::drivers::serial::SerialPort::put_u64(
                crate::usb::xhci::event::irq_count());
            crate::drivers::serial::SerialPort::puts(" ===\n");
        }

        #[cfg(target_arch = "x86_64")]
        {
            block_devices.extend(usb_block_devices);
            *crate::filesystems::blockdriver::driver::BLOCK_DEVICES.lock() = block_devices.clone();
        }

        // A: tmpfs mount via mount cap (P4-S5, §7.11 — no ambient VFS remaining)
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        {
            crate::filesystems::fstypes::register_all();
            let table = &crate::obj::bootstrap::boot_domain().table;
            let boot_end = crate::obj::bootstrap::boot_endowment();
            let args = crate::obj::Args {
                vals: alloc::vec![crate::obj::Value::Str("tmpfs"), crate::obj::Value::U64(0)],
            };
            match crate::obj::invoke(
                table,
                boot_end.mount,
                crate::obj::fs::MOUNT_CONTRACT,
                crate::obj::fs::MOUNT_HOOK,
                &args,
            ) {
                Ok(crate::obj::Reply::Caps(caps)) if !caps.is_empty() => {
                    log::info!("Mounted A> (tmpfs, via mount cap)");
                    // Create a test directory via the DirNode cap so fs-walk
                    // has something to exercise (P4-S3, §7.12.3).
                    let dir_cap = caps[0].id;
                    let mkdir_args = crate::obj::Args {
                        vals: alloc::vec![crate::obj::Value::Str("tmp")],
                    };
                    match crate::obj::invoke(
                        table,
                        dir_cap,
                        crate::obj::fs::DIR_CONTRACT,
                        crate::obj::fs::DIR_MKDIR,
                        &mkdir_args,
                    ) {
                        Ok(crate::obj::Reply::Caps(_)) => log::info!("Created A>tmp via DirNode mkdir cap"),
                        Ok(_) => log::warn!("mkdir A>tmp via cap: unexpected reply"),
                        Err(e) => log::warn!("mkdir A>tmp via cap failed: {:?}", e),
                    }
                }
                _ => log::warn!("A> tmpfs mount via cap failed"),
            }
        }

        // Mount the ESP via block-family + fs:mount caps (P4-S2, §7.11)
        #[cfg(target_arch = "x86_64")]
        {
            let table = &crate::obj::bootstrap::boot_domain().table;
            let boot_end = crate::obj::bootstrap::boot_endowment();
            let mounted = (|| {
                let first = match crate::obj::invoke(
                    table, boot_end.block,
                    crate::obj::fs::BLOCK_FAMILY_CONTRACT,
                    crate::obj::fs::BLOCK_FAMILY_FIRST,
                    &crate::obj::Args::none(),
                ) {
                    Ok(crate::obj::Reply::Caps(caps)) if !caps.is_empty() => caps[0].id,
                    _ => return false,
                };
                let args = crate::obj::Args {
                    vals: alloc::vec![crate::obj::Value::Str("fat32"), crate::obj::Value::U64(first.0)],
                };
                matches!(
                    crate::obj::invoke(
                        table, boot_end.mount,
                        crate::obj::fs::MOUNT_CONTRACT,
                        crate::obj::fs::MOUNT_HOOK,
                        &args,
                    ),
                    Ok(crate::obj::Reply::Caps(caps)) if !caps.is_empty()
                )
            })();
            if mounted {
                log::info!("Mounted ESP as B> (fat32, via mount cap)");
            } else {
                log::warn!("Could not mount ESP on B> via mount cap");
            }
        }

        // C8 — the device sweep is done; return to the boot domain before the
        // idle loop, which runs platform halt and xHCI hot-plug poll as the
        // boot domain again (§6.2).
        crate::obj::domain::set_current_domain(crate::obj::bootstrap::boot_domain());

        // P4 — post-mount separation proof: DirNode caps now exist in the
        // boot table; exercise the QUERY-only projection (§7.12.3).
        crate::obj::separation::run_post_mount();

        // P5 gate — the device sweep is the driver domain's last act; then the
        // P5 cascade/deny-list proofs run, followed by the §8.7 leak detector
        // (the gate is the test-suite: "run it after every test-suite
        // execution"). x86_64-only: `kerneldump` is not built on riscv64.
        #[cfg(target_arch = "x86_64")]
        {
            crate::obj::devices::materialize_pci_tree();
            crate::obj::revocation::run_p5_gate();

            let mut w = crate::drivers::serial::SerialPort;
            crate::kerneldump::graph_census(&mut w);
            crate::kerneldump::graph(&mut w);
            let leaked = crate::kerneldump::leak::leak_detect(&mut w);
            if leaked {
                crate::drivers::serial::SerialPort::puts("kerneldump leak_detect: FAIL\n");
            }
            crate::kerneldump::fs_walk(&mut w);
        }

        loop {
            #[cfg(target_arch = "x86_64")]
            {
                // Hot-plug: poll the retained xHCI controller for port
                // changes and register any newly attached block devices.
                let new_devices = crate::usb::xhci::poll();
                if !new_devices.is_empty() {
                    crate::filesystems::blockdriver::driver::BLOCK_DEVICES
                        .lock()
                        .extend(new_devices);
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
        if region.kind == crate::boot::MemoryRegionKind::Usable
            && region.size > best.1
        {
            best = (region.base, region.size);
        }
    }
    assert!(best.1 > 0, "no usable memory region found in memory map");
    best
}
