//! Boot/kernel hand-off types.
//!
//! `#[repr(C)]` guarantees a stable layout so the bootloader and kernel agree
//! on the memory representation even though they are compiled separately.

/// Memory region from the UEFI memory map.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    pub base: u64,
    pub size: u64,
    pub kind: MemoryRegionKind,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryRegionKind {
    Usable,
    Reserved,
    AcpiReclaimable,
    AcpiNvs,
    BootServicesCode,
    BootServicesData,
    LoaderCode,
    LoaderData,
}

/// Framebuffer information collected from UEFI GOP before `exit_boot_services`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    pub address: u64,
    pub width: usize,
    pub height: usize,
    /// Pixels per scanline (NOT bytes). Bytes per row = `stride * bpp`.
    pub stride: usize,
    pub pixel_format: PixelFormat,
    pub bpp: u8,
}

impl FramebufferInfo {
    pub const fn zeroed() -> Self {
        Self {
            address: 0,
            width: 0,
            height: 0,
            stride: 0,
            pixel_format: PixelFormat::Rgb,
            bpp: 0,
        }
    }

    /// Draw a solid-colour rectangle directly to the linear framebuffer.
    /// Safe to call with any `FramebufferInfo` — returns immediately if the
    /// address or bpp is invalid (zeroed fallback).  Writes pixel bytes in
    /// the correct order for `Rgb` vs `Bgr` formats.
    pub fn draw_rect(&self, x: usize, y: usize, w: usize, h: usize, r: u8, g: u8, b: u8) {
        if self.address == 0 || self.bpp < 3 {
            return;
        }
        let bpp = self.bpp as usize;
        let base = self.address as *mut u8;
        let row = self.stride * bpp;
        for dy in 0..h {
            for dx in 0..w {
                unsafe {
                    let off = (y + dy) * row + (x + dx) * bpp;
                    // GOP framebuffer memory is scanned out by the display
                    // controller outside the CPU's normal memory model.  Use
                    // volatile stores so early-boot diagnostic pixels cannot
                    // be folded away or reordered by the compiler.
                    match self.pixel_format {
                        PixelFormat::Rgb => {
                            base.add(off).write_volatile(r);
                            base.add(off + 1).write_volatile(g);
                            base.add(off + 2).write_volatile(b);
                        }
                        PixelFormat::Bgr => {
                            base.add(off).write_volatile(b);
                            base.add(off + 1).write_volatile(g);
                            base.add(off + 2).write_volatile(r);
                        }
                    }
                    if bpp >= 4 {
                        base.add(off + 3).write_volatile(0xFF);
                    }
                }
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb,
    Bgr,
}
