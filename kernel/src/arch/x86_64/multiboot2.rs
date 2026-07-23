use core::ptr::read_unaligned;

use crate::boot::{FramebufferInfo, MemoryRegion, MemoryRegionKind, PixelFormat};
use crate::Kernel;

core::arch::global_asm!(include_str!("multiboot2_header.s"));

const MB2_MAGIC: u32 = 0x36d76289;
const MAX_REGIONS: usize = 64;

unsafe fn r32(p: *const u8, off: usize) -> u32 {
    read_unaligned(p.add(off) as *const u32)
}
unsafe fn r64(p: *const u8, off: usize) -> u64 {
    read_unaligned(p.add(off) as *const u64)
}
unsafe fn r8(p: *const u8, off: usize) -> u8 {
    read_unaligned(p.add(off))
}
fn tag_next(tag: *const u8) -> *const u8 {
    let size = unsafe { r32(tag, 4) } as u64;
    let base = tag as u64;
    let aligned = (base + size + 7) & !7;
    aligned as *const u8
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_entry_mb2(magic: u32, info: *const u8) -> ! {
    if magic != MB2_MAGIC {
        loop { core::hint::spin_loop() }
    }

    let total_size = r32(info, 0);
    if total_size < 16 {
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
        let typ = r32(tag, 0);
        let size = r32(tag, 4);

        match typ {
            0 => break,
            6 if size >= 16 && region_count < MAX_REGIONS => {
                let entry_size = r32(tag, 8) as usize;
                let entries_base = unsafe { tag.add(16) };
                let data_size = (size - 16) as usize;
                let mut off = 0usize;
                while off + entry_size <= data_size && region_count < MAX_REGIONS {
                    let entry = unsafe { entries_base.add(off) };
                    let base = r64(entry, 0);
                    let len = r64(entry, 8);
                    let typ_ = r32(entry, 16);
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
                let addr = r64(tag, 8);
                let pitch = r32(tag, 16) as usize;
                let width = r32(tag, 20) as usize;
                let height = r32(tag, 24) as usize;
                let bpp_bits = r8(tag, 28);
                let bpp_bytes = bpp_bits / 8;
                let fb_type = r8(tag, 29);
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
            // without needing to map from a physical address.
            14 if size >= 28 => {
                let data = unsafe { core::slice::from_raw_parts(tag.add(8), (size - 8) as usize) };
                rsdp_data = Some(data);
            }
            15 if size >= 44 => {
                let data = unsafe { core::slice::from_raw_parts(tag.add(8), (size - 8) as usize) };
                rsdp_data = Some(data);
            }
            _ => {}
        }

        tag = tag_next(tag);
    }

    let memory_map: &'static [MemoryRegion] = unsafe {
        core::slice::from_raw_parts(&region_buf as *const MemoryRegion, region_count)
    };

    let stack_guard = unsafe { &crate::__stack_start as *const u8 as u64 - 4096 };

    let mut kernel = unsafe {
        Kernel::new(memory_map, &fb_info, stack_guard, 0, rsdp_data)
    };
    kernel.init();
    kernel.run();
}
