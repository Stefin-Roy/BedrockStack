#![no_main]
#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use uefi::prelude::*;
use uefi::boot::AllocateType;
use uefi::CStr16;
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::proto::console::gop::{GraphicsOutput, PixelBitmask, PixelFormat as UefiPixelFormat};
use uefi::proto::console::text::Output;
use uefi::fs::FileSystem;

mod allocator;
mod elf;
#[cfg(feature = "cpu_slow")]
mod limiter;

use common::serial::x86_64::SerialPort;
use common::types::{FramebufferInfo, MemoryRegion, MemoryRegionKind, PixelFormat};
use uefi::table::cfg::ConfigTableEntry;

#[global_allocator]
static ALLOCATOR: allocator::OsDataAllocator = allocator::OsDataAllocator;

/// Kernel stack size (64 KB).
const STACK_SIZE: usize = 64 * 1024;

/// UEFI page size.
const PAGE_SIZE: usize = 4096;

/// Write a message to both the serial port and the UEFI text console.
fn log_msg(output: &mut Output, msg: &str) {
    SerialPort::puts(msg);

    // Echo to UEFI console (ASCII bytes → UCS-2)
    let bytes = msg.as_bytes();
    let mut buf = [0u16; 200];
    let mut len = 0usize;
    for &b in bytes {
        if len >= buf.len() - 1 {
            break;
        }
        if b == b'\n' {
            if len < buf.len() - 1 {
                buf[len] = b'\r' as u16;
                len += 1;
            }
        }
        buf[len] = b as u16;
        len += 1;
    }
    buf[len] = 0;
    if let Ok(cs) = CStr16::from_u16_with_nul(&buf[..=len]) {
        let _ = output.output_string(cs);
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("FATAL: uefi::helpers::init() failed");

    // Init COM1 serial first — all log output goes here
    SerialPort::init();

    // 1. Open UEFI console output
    let handle_out = uefi::boot::get_handle_for_protocol::<Output>()
        .expect("FATAL: Output protocol not available");
    let mut output = uefi::boot::open_protocol_exclusive::<Output>(handle_out)
        .expect("FATAL: Output protocol open failed");

    // Clear the UEFI text console so boot output starts on a clean screen.
    let _ = output.clear();
    log_msg(&mut output, "[boot] BedrockOS booting...\n");

    // 2. Get framebuffer info from GOP
    log_msg(&mut output, "[boot] Querying GOP framebuffer...\n");
    let fb_info = get_framebuffer_info();
    // Full framebuffer details go to serial only (too long for console).
    SerialPort::puts("[boot] Framebuffer: addr=0x");
    SerialPort::put_hex(fb_info.address);
    SerialPort::puts(" w=");
    SerialPort::put_u64(fb_info.width as u64);
    SerialPort::puts(" h=");
    SerialPort::put_u64(fb_info.height as u64);
    SerialPort::puts(" stride=");
    SerialPort::put_u64(fb_info.stride as u64);
    SerialPort::puts("\n");
    log_msg(&mut output, "[boot] Framebuffer OK\n");

    // 3. Load kernel ELF from disk (allocates a large OS_DATA buffer).
    log_msg(&mut output, "[boot] Reading kernel from disk: \\EFI\\BEDROCK\\KERNEL\n");

    let kernel_data = load_file_from_disk(cstr16!(r"\EFI\BEDROCK\KERNEL").into());
    SerialPort::puts("[boot] Kernel file read: ");
    SerialPort::put_u64(kernel_data.len() as u64);
    SerialPort::puts(" bytes\n");
    log_msg(&mut output, "[boot] Kernel read from disk\n");

    log_msg(&mut output, "[boot] Parsing ELF and loading segments...\n");
    let entry = unsafe { elf::load_elf(&kernel_data).expect("FATAL: kernel ELF corrupt or invalid") };
    SerialPort::puts("[boot] Kernel entry: 0x");
    SerialPort::put_hex(entry);
    SerialPort::puts("\n");
    log_msg(&mut output, "[boot] Kernel loaded\n");

    // 4. Allocate transfer buffers using the OS_DATA allocator BEFORE reading
    //    the final memory map. The memory map is only built AFTER
    //    exit_boot_services so that these allocations (and the kernel image)
    //    are reflected correctly and the kernel never hands out frames that
    //    hold its own stack / hand-off data.
    log_msg(&mut output, "[boot] Allocating transfer buffers (OS_DATA)...\n");

    // Estimate capacity for the region list from the current map, with generous
    // slack for entries added/split by our own allocations before
    // exit_boot_services. This buffer CANNOT be grown after exit (the allocator
    // is gone), so we over-provision and hard-fail on overflow rather than
    // silently truncating the map.
    let est_entries = uefi::boot::memory_map(MemoryType::LOADER_DATA)
        .map(|m| m.len())
        .unwrap_or(0);
    let mut regions_buf: Vec<MemoryRegion> = Vec::with_capacity(est_entries * 2 + 256);
    let fb_buf: Vec<FramebufferInfo> = alloc::vec![fb_info];

    let fb_ptr = fb_buf.as_ptr();

    // Allocate the kernel stack as whole pages with one extra page at the bottom
    // used as a guard page. The kernel leaves the guard page unmapped, so a
    // stack overflow faults (caught by the double-fault IST) instead of silently
    // corrupting adjacent memory. OS_DATA keeps it reserved after exit.
    let stack_pages = STACK_SIZE / PAGE_SIZE + 1; // +1 guard page
    let stack_base = uefi::boot::allocate_pages(
        AllocateType::AnyPages,
        allocator::OS_DATA,
        stack_pages,
    )
    .expect("FATAL: kernel stack allocation failed (insufficient memory)")
    .as_ptr() as usize;
    let stack_guard = stack_base as u64; // lowest page is the guard
    let stack_region_top = stack_base + stack_pages * PAGE_SIZE;

    // Stack grows downward. Align the top DOWN to 16 bytes, then subtract 8 to
    // emulate the post-`call` stack state the calling convention expects at
    // function entry (RSP % 16 == 8 on x86_64 SysV; RISC-V uses a0-a7).
    let stack_top = (((stack_region_top) & !0xF) - 8) as *const u8;

    // 5. Find ACPI RSDP in the UEFI configuration table (before exit_boot_services).
    let rsdp_addr = find_rsdp(&mut output);

    // 6. Print final message before exiting boot services.
    log_msg(&mut output, "[boot] Jumping to kernel...\n");

    // ── Framebuffer stroboscope markers (no serial needed) ──
    fb_info.draw_rect(0, 0, 64, 64, 255, 0, 0);   // M1: RED — before exit_boot_services

    // 7. Exit boot services — after this, only runtime services remain.
    //    The returned map is the authoritative post-exit memory map.
    //    uefi-rs handles the memory-map-key dance internally; if the
    //    firmware refuses the transition the library will panic with a
    //    clear message rather than leaving us in an inconsistent state.
    let mmap = unsafe { uefi::boot::exit_boot_services(Some(allocator::OS_DATA)) };

    fb_info.draw_rect(64, 0, 64, 64, 0, 255, 0);  // M2: GREEN — after exit_boot_services

    // 8. Build the region list from the FINAL map into the pre-allocated buffer.
    //    No allocation happens here (we stay within reserved capacity), which is
    //    required since boot services (and thus the allocator) are gone.
    //    NOTE: UEFI console is gone after exit_boot_services —
    //          all further output is serial-only.
    for desc in mmap.entries() {
        if desc.page_count == 0 {
            continue;
        }
        // SAFETY/ROBUSTNESS: OVMF/QEMU commonly report gigantic "conventional"
        // regions in the high address space (e.g. 12 GiB @ 0xfd00000000) that are
        // NOT backed by real RAM. Mapping or allocating from them makes the kernel
        // fabricate page tables for nonexistent memory. On real hardware these
        // >4 GiB regions are genuine RAM — we only filter them under hypervisors
        // (detected via CPUID hypervisor bit).
        #[cfg(target_arch = "x86_64")]
        if is_hypervisor() && desc.ty == MemoryType::CONVENTIONAL && desc.phys_start >= 0x1_0000_0000 {
            continue;
        }
        // Cannot grow the buffer after exit_boot_services. If we ever exceed the
        // reserved capacity, halt loudly instead of silently dropping regions
        // (which would let the kernel hand out reserved frames).
        if regions_buf.len() == regions_buf.capacity() {
            SerialPort::puts("[boot] FATAL: memory map exceeded reserved capacity\n");
            loop {
                core::hint::spin_loop();
            }
        }
        let kind = classify_memory(desc.ty);
        regions_buf.push(MemoryRegion {
            base: desc.phys_start,
            size: desc.page_count * 4096,
            kind,
        });
    }

    let regions_ptr = regions_buf.as_ptr();
    let regions_len = regions_buf.len();

    core::mem::forget(regions_buf);
    core::mem::forget(fb_buf);

    // 9. We are now bare metal. Jump to kernel.
    // NOTE: Serial I/O still works after exit_boot_services (bare metal port I/O).
    SerialPort::puts("[boot] Boot services exited. Jumping to kernel...\n");

    fb_info.draw_rect(128, 0, 64, 64, 0, 0, 255);  // M3: BLUE — before jump_to_kernel

    #[cfg(feature = "cpu_slow")]
    {
        SerialPort::puts("[boot] Enabling CPU slow mode...\n");
        unsafe { limiter::enable_cpu_slow_mode() };
    }

    unsafe {
        jump_to_kernel(entry, stack_top, regions_ptr, regions_len, fb_ptr, stack_guard, rsdp_addr);
    }
}

/// Detect whether the CPU is running under a hypervisor (KVM, Hyper-V, etc.)
/// via the CPUID hypervisor present bit (ECX bit 31 of leaf 1).
#[cfg(target_arch = "x86_64")]
fn is_hypervisor() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    (result.ecx >> 31) & 1 != 0
}

/// Classify a UEFI memory type into our kernel-facing region kind.
///
/// Only `CONVENTIONAL` memory is reported as `Usable`. Everything else —
/// including our custom `OS_DATA` (holding the kernel stack and hand-off
/// buffers), ACPI, loader and boot-services memory, and any unknown/MMIO type —
/// is reported as `Reserved` so the kernel's frame allocator never hands it out.
fn classify_memory(ty: MemoryType) -> MemoryRegionKind {
    match ty {
        MemoryType::CONVENTIONAL => MemoryRegionKind::Usable,
        MemoryType::ACPI_RECLAIM => MemoryRegionKind::AcpiReclaimable,
        MemoryType::ACPI_NON_VOLATILE => MemoryRegionKind::AcpiNvs,
        MemoryType::BOOT_SERVICES_CODE => MemoryRegionKind::BootServicesCode,
        MemoryType::BOOT_SERVICES_DATA => MemoryRegionKind::BootServicesData,
        MemoryType::LOADER_CODE => MemoryRegionKind::LoaderCode,
        MemoryType::LOADER_DATA => MemoryRegionKind::LoaderData,
        _ => MemoryRegionKind::Reserved,
    }
}

/// Find the physical address of the ACPI 2.0 RSDP from the UEFI config table.
/// Returns 0 if not found.
fn find_rsdp(output: &mut Output) -> u64 {
    uefi::system::with_config_table(|entries| {
        for entry in entries {
            if entry.guid == ConfigTableEntry::ACPI2_GUID {
                let addr = entry.address as u64;
                SerialPort::puts("[boot] ACPI RSDP at 0x");
                SerialPort::put_hex(addr);
                SerialPort::puts("\n");
                log_msg(output, "[boot] ACPI RSDP OK\n");
                return addr;
            }
        }
        log_msg(output, "[boot] WARNING: ACPI RSDP not found\n");
        0
    })
}

/// Parse UEFI `PixelBitmask` into our `PixelFormat` and bytes-per-pixel.
///
/// The bitmask tells us which bits in a 32-bit pixel correspond to each
/// channel.  For byte-aligned masks (the common case 24/32 bpp) we derive
/// the byte order and bpp directly.  Non-byte-aligned masks (e.g. 16-bit
/// 5:6:5) are logged and mapped to BGR 32bpp as a reasonable fallback.
fn parse_bitmask(bm: &PixelBitmask) -> (PixelFormat, u8) {
    // Check whether every mask is byte-aligned (all set bits fit in one byte).
    let mask_aligned = |mask: u32| -> bool {
        if mask == 0 {
            return true;
        }
        let tz = mask.trailing_zeros();
        let width = 32 - mask.leading_zeros() - tz;
        tz % 8 == 0 && width <= 8
    };

    if !mask_aligned(bm.red)
        || !mask_aligned(bm.green)
        || !mask_aligned(bm.blue)
    {
        SerialPort::puts("[boot] WARNING: non-byte-aligned pixel bitmask (red=0x");
        SerialPort::put_hex(bm.red as u64);
        SerialPort::puts(" green=0x");
        SerialPort::put_hex(bm.green as u64);
        SerialPort::puts(" blue=0x");
        SerialPort::put_hex(bm.blue as u64);
        SerialPort::puts(" reserved=0x");
        SerialPort::put_hex(bm.reserved as u64);
        SerialPort::puts(") — using BGR 32bpp fallback\n");
        return (PixelFormat::Bgr, 4);
    }

    // Byte offset of each channel in the pixel DWORD (0 = LSB, 3 = MSB).
    let r_byte = (bm.red.trailing_zeros() / 8) as u8;
    let _g_byte = (bm.green.trailing_zeros() / 8) as u8;
    let b_byte = (bm.blue.trailing_zeros() / 8) as u8;

    // If Blue occupies a lower byte than Red, the native order is BGR.
    let format = if b_byte < r_byte {
        PixelFormat::Bgr
    } else {
        PixelFormat::Rgb
    };

    // Compute bpp from the highest bit used across all channels.
    let combined = bm.red | bm.green | bm.blue | bm.reserved;
    let bpp: u8 = if combined == 0 {
        4
    } else {
        let max_bit = 32 - combined.leading_zeros(); // u32
        ((max_bit + 7) / 8).clamp(2, 4) as u8
    };

    (format, bpp)
}

/// Get framebuffer information from UEFI GOP.
/// Never panics — if GOP is unavailable or has no linear framebuffer,
/// returns a zeroed `FramebufferInfo` so the kernel can fall back to
/// text mode or serial-only operation.
fn get_framebuffer_info() -> FramebufferInfo {
    let handle = match uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        Ok(h) => h,
        Err(_) => {
            SerialPort::puts("[boot] GOP not available — continuing without framebuffer\n");
            return FramebufferInfo::zeroed();
        }
    };

    let mut gop = match uefi::boot::open_protocol_exclusive::<GraphicsOutput>(handle) {
        Ok(g) => g,
        Err(_) => {
            SerialPort::puts("[boot] GOP open failed — continuing without framebuffer\n");
            return FramebufferInfo::zeroed();
        }
    };

    let mode = gop.current_mode_info();
    let (width, height) = mode.resolution();
    let stride = mode.stride();

    let (pixel_format, bpp) = match mode.pixel_format() {
        UefiPixelFormat::Rgb => (PixelFormat::Rgb, 4u8),
        UefiPixelFormat::Bgr => (PixelFormat::Bgr, 4u8),
        // Bitmask has a real linear framebuffer but the channel layout is
        // encoded in the mode's pixel bitmasks — parse them.
        UefiPixelFormat::Bitmask => {
            if let Some(ref bm) = mode.pixel_bitmask() {
                parse_bitmask(bm)
            } else {
                SerialPort::puts("[boot] WARNING: GOP Bitmask without pixel_bitmask() — using BGR 32bpp\n");
                (PixelFormat::Bgr, 4)
            }
        }
        // BltOnly provides NO linear framebuffer address — let the kernel
        // run without a framebuffer rather than crashing at boot time.
        UefiPixelFormat::BltOnly => {
            SerialPort::puts("[boot] GOP is BltOnly (no linear framebuffer) — continuing without framebuffer\n");
            return FramebufferInfo::zeroed();
        }
    };

    let mut fb = gop.frame_buffer();
    let address = fb.as_mut_ptr() as u64;

    FramebufferInfo {
        address,
        width,
        height,
        stride,
        pixel_format,
        bpp,
    }
}

/// Load a file from the boot partition's FAT32 filesystem.
/// Halts with a clear message on failure instead of panicking.
fn load_file_from_disk(path: &uefi::fs::Path) -> Vec<u8> {
    let ss = match uefi::boot::get_image_file_system(uefi::boot::image_handle()) {
        Ok(s) => s,
        Err(_) => {
            SerialPort::puts("[boot] FATAL: ESP file system protocol unavailable\n");
            loop { core::hint::spin_loop() }
        }
    };
    let mut fs = FileSystem::new(ss);
    match fs.read(path) {
        Ok(data) => data,
        Err(_) => {
            SerialPort::puts("[boot] FATAL: kernel not found at \\EFI\\BEDROCK\\KERNEL\n");
            loop { core::hint::spin_loop() }
        }
    }
}

/// Jump to kernel entry point.
///
/// # Safety
/// - entry must be a valid kernel entry point
/// - stack_top must point to valid writable memory (grows downward)
/// - regions_ptr must point to valid MemoryRegion array of length regions_len
/// - fb_ptr must point to valid FramebufferInfo
/// - stack_guard is the physical address of the (unmapped-by-kernel) guard page
/// - rsdp_addr is the physical address of the ACPI RSDP (0 if unknown)
/// - This function does not return
unsafe fn jump_to_kernel(
    entry: u64,
    stack_top: *const u8,
    regions_ptr: *const MemoryRegion,
    regions_len: usize,
    fb_ptr: *const FramebufferInfo,
    stack_guard: u64,
    rsdp_addr: u64,
) -> ! {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            // Firmware state must not leak into the kernel entry ABI.  In
            // particular, an IRQ between the stack switch and IDT setup would
            // use a firmware IDT with our new stack (or fault outright), which
            // commonly looks like a silent hang/triple fault on real hardware.
            // The kernel enables interrupts only after its GDT and IDT exist.
            "cli",
            // Rust/SysV code assumes the direction flag is clear.  UEFI
            // firmware should leave it clear, but make the bare-metal
            // hand-off independent of that promise.
            "cld",
            "mov rsp, r10",
            "xor rbp, rbp",
            "jmp r9",
            in("r10") stack_top,
            in("r9") entry,
            in("rdi") regions_ptr,
            in("rsi") regions_len,
            in("rdx") fb_ptr,
            in("rcx") stack_guard,
            in("r8") rsdp_addr,
            options(noreturn)
        );
    }
}
