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
    /// Pixels per scanline (NOT bytes). Bytes per row = `stride * 4`.
    pub stride: usize,
    pub pixel_format: PixelFormat,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb,
    Bgr,
}
