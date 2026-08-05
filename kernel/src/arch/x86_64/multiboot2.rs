use core::ptr::read_unaligned;

use crate::boot::{FramebufferInfo, MemoryRegion, MemoryRegionKind, PixelFormat};
use crate::drivers::serial::SerialPort;
use crate::Kernel;

core::arch::global_asm!(include_str!("multiboot2_header.s"));

const MB2_MAGIC: u32 = 0x36d76289;
const MAX_REGIONS: usize = 64;

// ── RSDP stash (Phase 5) ──────────────────────────────────────────────
// Tags 14/15 embed the RSDP bytes in the LOW multiboot2 info buffer, which
// is identity-mapped before `switch_to_higher_half` but stripped from the
// page tables afterwards.  ACPI parses `rsdp_data` only after the switch, so
// the bytes must be copied into a kernel-resident ('static) buffer here.
const RSDP_BUF_SIZE: usize = 512; // RSDP v2 is ~44 bytes; 512 is ample.

/// Shared wrapper so the RSDP stash links without a `static mut`.
/// Written once (single-threaded BSP, pre-SMP) and read-only afterwards.
struct Shared<T>(core::cell::UnsafeCell<T>);

unsafe impl<T> Sync for Shared<T> {}
unsafe impl<T> Send for Shared<T> {}

static RSDP_BUF: Shared<[u8; RSDP_BUF_SIZE]> =
    Shared(core::cell::UnsafeCell::new([0u8; RSDP_BUF_SIZE]));
static RSDP_LEN: Shared<usize> = Shared(core::cell::UnsafeCell::new(0));

/// Copy `data` into the static RSDP stash and return it as `'static`.
///
/// # Safety
/// Single-threaded (BSP, pre-SMP).  The stash is written once and read-only
/// afterwards.
unsafe fn stash_rsdp(data: &[u8]) -> &'static [u8] {
    assert!(
        data.len() <= RSDP_BUF_SIZE,
        "RSDP too large for stash ({})",
        data.len()
    );
    let buf = RSDP_BUF.0.get();
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len());
        *RSDP_LEN.0.get() = data.len();
        core::slice::from_raw_parts(buf as *const u8, *RSDP_LEN.0.get())
    }
}

unsafe fn r32(p: *const u8, off: usize) -> u32 {
    unsafe { read_unaligned(p.add(off) as *const u32) }
}
unsafe fn r64(p: *const u8, off: usize) -> u64 {
    unsafe { read_unaligned(p.add(off) as *const u64) }
}
unsafe fn r8(p: *const u8, off: usize) -> u8 {
    unsafe { read_unaligned(p.add(off)) }
}
fn tag_next(tag: *const u8) -> *const u8 {
    let size = unsafe { r32(tag, 4) } as u64;
    let base = tag as u64;
    let aligned = (base + size + 7) & !7;
    aligned as *const u8
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_entry_mb2(magic: u32, info: *const u8) -> ! {
    // NOTE (Phase 3): this entry runs BEFORE `switch_to_higher_half`, while
    // the static `.boottables` (CR3 = `__boot_pml4`) identity map is still
    // live.  `info` is the low physical pointer GRUB passed us — its value
    // is < 1 GiB, so direct dereference below works via the identity map.
    // `to_physmap()` also returns identity while PHYS_MAP_ON is false, so the
    // handoff pointers are never double-translated.  Do NOT move the tag
    // parsing after the higher-half switch without adding phys→virt fixes.
    // Initialise COM1 serial as early as possible so diagnostics are
    // available even if a tag parse or allocation panics later.
    SerialPort::init();
    SerialPort::puts("[mb2] rust_entry_mb2 entered\n");

    if magic != MB2_MAGIC {
        SerialPort::puts("[mb2] ERROR: bad multiboot2 magic\n");
        loop { core::hint::spin_loop() }
    }

    let total_size = unsafe { r32(info, 0) };
    if total_size < 16 {
        SerialPort::puts("[mb2] ERROR: multiboot2 info too small\n");
        loop { core::hint::spin_loop() }
    }

    let mut fb_info: FramebufferInfo = FramebufferInfo {
        address: 0,
        width: 80,
        height: 25,
        stride: 80,
        pixel_format: PixelFormat::Bgr,
        bpp: 1,
    };
    let mut rsdp_data: Option<&'static [u8]> = None;

    let mut region_buf: [MemoryRegion; MAX_REGIONS] = unsafe { core::mem::zeroed() };
    let mut region_count: usize = 0;

    let mut tag = unsafe { info.add(8) };
    loop {
        let typ = unsafe { r32(tag, 0) };
        let size = unsafe { r32(tag, 4) };

        match typ {
            0 => break,
            6 if size >= 16 && region_count < MAX_REGIONS => {
                let entry_size = unsafe { r32(tag, 8) } as usize;
                let entries_base = unsafe { tag.add(16) };
                let data_size = (size - 16) as usize;
                let mut off = 0usize;
                while off + entry_size <= data_size && region_count < MAX_REGIONS {
                    let entry = unsafe { entries_base.add(off) };
                    let base = unsafe { r64(entry, 0) };
                    let len = unsafe { r64(entry, 8) };
                    let typ_ = unsafe { r32(entry, 16) };
                    if len > 0 {
                        let kind = match typ_ {
                            1 => MemoryRegionKind::Usable,
                            3 => MemoryRegionKind::AcpiReclaimable,
                            4 => MemoryRegionKind::AcpiNvs,
                            _ => MemoryRegionKind::Reserved,
                        };
                        region_buf[region_count] = MemoryRegion {
                            base,
                            size: len,
                            kind,
                        };
                        region_count += 1;
                    }
                    off += entry_size;
                }
            }
            8 if size >= 32 => {
                let addr = unsafe { r64(tag, 8) };
                let pitch = unsafe { r32(tag, 16) } as usize;
                let width = unsafe { r32(tag, 20) } as usize;
                let height = unsafe { r32(tag, 24) } as usize;
                let bpp_bits = unsafe { r8(tag, 28) };
                if bpp_bits == 0 {
                    SerialPort::puts("[mb2] WARNING: framebuffer tag with bpp_bits=0, skipping\n");
                    break;
                }
                let bpp_bytes = bpp_bits / 8;
                let fb_type = unsafe { r8(tag, 29) };
                let pixel_format = match fb_type {
                    2 => PixelFormat::Rgb,
                    _ => PixelFormat::Bgr,
                };
                fb_info = FramebufferInfo {
                    address: addr,
                    width,
                    height,
                    stride: pitch / bpp_bytes as usize,
                    pixel_format,
                    bpp: bpp_bytes,
                };
            }
            // Multiboot2 tags 14 (ACPI_OLD_RSDP) and 15 (ACPI_NEW_RSDP)
            // embed the *entire* RSDP table data at `tag + 8`, NOT a
            // pointer to it.  Extract the embedded bytes and pass them
            // as a data slice so `parse_tables_from_data` can parse them
            // without needing to map from a physical address.  The bytes
            // are copied into the kernel-resident stash because the low
            // multiboot2 info buffer is unmapped after the higher-half
            // switch, and ACPI parses this data only afterwards.
            14 if size >= 28 => {
                let data = unsafe { core::slice::from_raw_parts(tag.add(8), (size - 8) as usize) };
                rsdp_data = Some(unsafe { stash_rsdp(data) });
            }
            15 if size >= 44 => {
                let data = unsafe { core::slice::from_raw_parts(tag.add(8), (size - 8) as usize) };
                rsdp_data = Some(unsafe { stash_rsdp(data) });
            }
            _ => {}
        }

        tag = tag_next(tag);
    }

    let memory_map: &'static [MemoryRegion] = unsafe {
        core::slice::from_raw_parts(&region_buf as *const MemoryRegion, region_count)
    };

    // The guard page is the physical frame just below the kernel's `.stack`
    // section.  `__stack_start` is a higher-half VMA; `__stack_start_phys`
    // is the LMA (physical) value the allocator and pager compare against.
    let stack_guard = unsafe { &crate::__stack_start_phys as *const u8 as u64 - 4096 };

    let mut kernel = unsafe {
        Kernel::new(memory_map, &fb_info, stack_guard, 0, rsdp_data)
    };
    kernel.init();
    kernel.run();
}
