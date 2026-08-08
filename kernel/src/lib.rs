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
pub mod proc;
#[cfg(target_arch = "x86_64")]
pub mod usb;
pub mod smp;
pub mod services;
#[cfg(target_arch = "x86_64")]
pub mod syscall;

use acpi::AcpiSubsystem;
use arch::CurrentArch;
use boot::{FramebufferInfo, MemoryRegion};
use framebuffer::Framebuffer;
use services::irqsafe::IrqLock;

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
    pub idt_start: u64,
    pub idt_end: u64,
}

pub struct Kernel {
    framebuffer: IrqLock<Framebuffer>,
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
                idt_start: &__idt_start as *const u8 as u64,
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
            framebuffer: IrqLock::new(display),
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

        let size = self.framebuffer.lock().total_bytes();
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
        self.framebuffer.lock().set_shadow_va(va);
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
        let (height, stride, bpp) = {
            let fb = self.framebuffer.lock();
            (fb.height(), fb.stride(), fb.bpp())
        };
        let vmm = CurrentArch::setup_virt_mem(
            &mut self.allocator,
            &self.layout,
            self.stack_guard,
            self.fb_phys,
            height,
            stride,
            bpp,
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
            // Materialize one PciDeviceNode child per discovered device under
            // the pci:forest root, so the device complex is capability-visible
            // (count/children hooks, cascade severance) from boot (§3.7.2,
            // §7.11.4). No-op on riscv64 (the PCI subsystem is x86_64-only).
            crate::obj::devices::materialize_pci_tree();
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
        let mut block_devices = crate::filesystems::blockdriver::driver::init_all(
            crate::pci::devices(),
        );

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
            block_devices.extend(usb_block_devices);
        }

        // A: tmpfs mount via mount cap (CapabilityVfs step 5, §7.11 — no ambient VFS remaining)
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        {
            crate::filesystems::fstypes::register_all();

            // Bring-up registration: push every discovered block device
            // (AHCI + the xHCI-attached ones merged above) into the
            // block-family interior through the `register` hook — the
            // family node materializes later from its own interior, never an
            // ambient list (§7.11.4).
            #[cfg(target_arch = "x86_64")]
            {
                for dev in block_devices.iter() {
                    register_block_device(dev.clone());
                }
            }

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
                    #[cfg(feature = "selftest")]
                    {
                        // Create a test directory via the DirNode cap so fs-walk
                        // has something to exercise (CapabilityVfs step 3, §7.12.3).
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
                }
                _ => log::warn!("A> tmpfs mount via cap failed"),
            }
        }

        // Mount the ESP via block-family + fs:mount caps (CapabilityVfs step 2, §7.11)
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

        // Top up the pre-built `mem:region` wrapper pools once the boot-time
        // allocation work (mounts, PCI tree, revocation gate) has consumed the
        // bootstrap stock, so allocator hooks keep handing out region wrappers
        // with zero allocation during the idle loop. Safe point: no memory hook
        // is on the stack here.
        crate::obj::memregion::replenish(crate::obj::memregion::RegionKind::Phys, 32);
        crate::obj::memregion::replenish(crate::obj::memregion::RegionKind::Heap, 32);

        // Boot-time test-suite output: the §8.7 leak detector plus graph and
        // fs census dumps. x86_64-only (`kerneldump` is not built on riscv64);
        // gated under the `selftest` feature, off by default. Runs before the
        // scheduler takes over (the scheduler never returns).
        #[cfg(all(target_arch = "x86_64", feature = "selftest"))]
        {
            let mut w = crate::drivers::serial::SerialPort;
            crate::kerneldump::graph_census(&mut w);
            crate::kerneldump::graph(&mut w);
            let leaked = crate::kerneldump::leak::leak_detect(&mut w);
            if leaked {
                crate::drivers::serial::SerialPort::puts("kerneldump leak_detect: FAIL\n");
            }
            crate::kerneldump::fs_walk(&mut w);
        }

        // Initialize syscall MSRs and hand the BSP to the multitask scheduler.
        // The scheduler spawns the two boot tasks from B:\EFI\BEDROCK\INIT and
        // owns the BSP forever; its idle path (xHCI hot-plug poll + halt)
        // replaces the old post-init idle loop, so this never returns.
        #[cfg(target_arch = "x86_64")]
        {
            crate::arch::x86_64::syscall::init();
            crate::proc::run(&mut self.allocator, self.page_table_root);
        }
        // riscv64 has no userspace scheduler: idle (halt with IRQs enabled)
        // forever. `proc::run` is x86_64-only.
        #[cfg(target_arch = "riscv64")]
        loop {
            crate::arch::CurrentArch::enable_interrupts();
            crate::arch::CurrentArch::halt();
        }
    }
}

/// Register a block device into the `block:family` interior via the
/// `register` hook (§7.11.4): wrap the device in a `BlockNode` cap inserted
/// into the boot table, then invoke the hook with that cap's id. The family
/// node resolves the cap, downcasts to `BlockNode`, and pushes the device
/// into its interior — the kernel's own bring-up path, no ambient list.
#[cfg(target_arch = "x86_64")]
pub(crate) fn register_block_device(
    device: alloc::sync::Arc<dyn crate::filesystems::blockdriver::traits::BlockDevice>,
) {
    use crate::obj::bootstrap::{boot_domain, boot_endowment};
    use crate::obj::fs::{BLOCK_FAMILY_CONTRACT, BLOCK_FAMILY_REGISTER, BlockNode};
    use crate::obj::{
        invoke, Args, CapHandle, CapId, CapRights, ContractRights, HandleState, Rights, Value,
    };

    let table = &boot_domain().table;
    let cap = table.insert(CapHandle {
        id: CapId(0),
        node: alloc::sync::Arc::new(BlockNode::new(device)),
        rights: CapRights::new(Rights::INVOKE, ContractRights::empty()),
        state: HandleState::Live,
    });
    let args = Args { vals: alloc::vec![Value::U64(cap.0)] };
    match invoke(
        table,
        boot_endowment().block,
        BLOCK_FAMILY_CONTRACT,
        BLOCK_FAMILY_REGISTER,
        &args,
    ) {
        Ok(_) => {}
        Err(e) => log::warn!("block:family register failed: {:?}", e),
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
