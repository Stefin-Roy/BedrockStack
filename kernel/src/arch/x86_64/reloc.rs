//! PIE VMA-only self-relocation.
//!
//! The kernel is ET_DYN static PIE with LMA fixed at `__low_end` via `AT()` in
//! `kernel/linker.ld`. GRUB and the UEFI loader copy PT_LOAD to `p_paddr`
//! and jump to low `_start` identity; they do NOT apply RELA. The post-link
//! strip hides SHT_RELA from GRUB (see `kernel/build.rs` + `create_image.py`)
//! but the PT_LOAD data remains.
//!
//! The high region `[KERNEL_VMA, +256MiB)` slides to `KASLR_BASE =
//! KERNEL_VMA - kaslr`. Only that high window is slid — low `0x400000`
//! stays identity. The walker filters `r_offset >= KERNEL_VMA_BASE` so low
//! `.text.boot/.boottables` absolutes stay at link KERNEL_VMA (fixed
//! .boottables until slid pagetables).
//!
//! Called from `Kernel::init` after `random::init_early` + `init_kaslr`
//! while still on `.boottables` (fixed `KERNEL_VMA` + low `1GiB` identity),
//! but before `switch_to_higher_half` builds `Vmm`. It patches the in-memory
//! image at LMA so after paging the slid `VMA` fetches slid pointers.

use crate::mm::layout::KERNEL_VMA_BASE;

#[repr(C)]
struct Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

const R_X86_64_64: u32 = 1;
const R_X86_64_RELATIVE: u32 = 8;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;
const KERNEL_IMAGE_SIZE: u64 = 0x1000_0000; // 256 MiB — layout::KERNEL_IMAGE_SIZE

unsafe extern "C" {
    static __rela_dyn_start_phys: u8;
    static __rela_dyn_end_phys: u8;
    static __low_end: u8;
}

/// Apply RELA with VMA-only filter: only high window, only absolute types.
/// `kaslr` is passed in to avoid loading KASLR_OFFSET via high VMA after patch.
pub unsafe fn apply_kaslr(kaslr: u64) {
    if kaslr == 0 {
        return;
    }
    let slide = (kaslr_base(kaslr) as i64).wrapping_sub(KERNEL_VMA_BASE as i64);
    // Rela entries live at LMA (__low_end+(VMA-KERNEL_VMA)) — use phys symbols
    // via identity so the walk itself is not affected by the patch.
    let rela_start = unsafe { &__rela_dyn_start_phys as *const u8 as u64 };
    let rela_end = unsafe { &__rela_dyn_end_phys as *const u8 as u64 };
    let count = if rela_end > rela_start {
        (rela_end - rela_start) / core::mem::size_of::<Rela>() as u64
    } else {
        0
    };
    if count == 0 {
        return;
    }
    let low_end = unsafe { &__low_end as *const u8 as u64 };
    let high_start = KERNEL_VMA_BASE;
    let high_end = KERNEL_VMA_BASE + KERNEL_IMAGE_SIZE;
    let mut i = 0u64;
    while i < count {
        let rela = unsafe { &*((rela_start + i * 24) as *const Rela) };
        let r_offset = rela.r_offset;
        let r_type = (rela.r_info & 0xFFFF_FFFF) as u32;
        // VMA-only: only high r_offset, and only absolute types.
        // PC32/PLT32/GOTPCREL are pc-relative and invariant under slide.
        if r_offset >= high_start && r_offset < high_end {
            let lma = low_end.wrapping_add(r_offset.wrapping_sub(KERNEL_VMA_BASE));
            match r_type {
                R_X86_64_RELATIVE | R_X86_64_64 => {
                    let ptr = lma as *mut u64;
                    unsafe {
                        let cur = core::ptr::read_volatile(ptr);
                        core::ptr::write_volatile(ptr, cur.wrapping_add(slide as u64));
                    }
                }
                R_X86_64_32 | R_X86_64_32S => {
                    let ptr = lma as *mut u32;
                    unsafe {
                        let cur = core::ptr::read_volatile(ptr);
                        core::ptr::write_volatile(ptr, cur.wrapping_add(slide as u32));
                    }
                }
                _ => {
                    // Ignore pc-relative and other types — they are
                    // position-relative and don't need slide, or are
                    // non-ALLOC debug entries that were discarded.
                }
            }
        }
        i += 1;
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

fn kaslr_base(kaslr: u64) -> u64 {
    KERNEL_VMA_BASE.wrapping_sub(kaslr)
}
