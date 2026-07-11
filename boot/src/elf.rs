//! Minimal ELF64 parser for loading kernel binaries.
//!
//! Only handles ELF64 little-endian executables.  The expected `e_machine`
//! value is set per target architecture so the bootloader validates that the
//! kernel it loads matches the platform it runs on.
//! Copies LOAD segments to their physical addresses after reserving that
//! physical range from UEFI, so firmware/boot-services allocations can never
//! sit under the kernel image and get clobbered by the copy.

use uefi::boot::{self, AllocateType};
use uefi::mem::memory_map::MemoryType;

/// ELF64 magic bytes.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// 4 KiB UEFI page size.
const PAGE_SIZE: u64 = 4096;

/// ELF64 header (64 bytes).
#[repr(C)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// PT_LOAD segment type.
const PT_LOAD: u32 = 1;

/// Read a little-endian u16 from a byte slice at the given offset.
fn read_u16(data: &[u8], offset: usize) -> u16 {
    debug_assert!(offset + 1 < data.len(), "read_u16 out of bounds");
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

/// Read a little-endian u32 from a byte slice at the given offset.
fn read_u32(data: &[u8], offset: usize) -> u32 {
    debug_assert!(offset + 3 < data.len(), "read_u32 out of bounds");
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Read a little-endian u64 from a byte slice at the given offset.
fn read_u64(data: &[u8], offset: usize) -> u64 {
    debug_assert!(offset + 7 < data.len(), "read_u64 out of bounds");
    u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

/// Parse and load an ELF64 binary into physical memory.
///
/// Returns the entry point address on success.
///
/// # Safety
/// - `elf_data` must point to a valid ELF64 binary.
/// - LOAD segments will be copied to their physical addresses (p_paddr).
/// - Caller must ensure target memory is writable and not overlapping critical regions.
pub unsafe fn load_elf(elf_data: &[u8]) -> Result<u64, &'static str> {
    // Validate minimum size
    if elf_data.len() < core::mem::size_of::<Elf64Ehdr>() {
        return Err("ELF too small");
    }

    // Validate magic
    if elf_data[..4] != ELF_MAGIC {
        return Err("Invalid ELF magic");
    }

    // Validate ELF64 (class = 2), little-endian (data = 1)
    if elf_data[4] != 2 {
        return Err("Not ELF64");
    }
    if elf_data[5] != 1 {
        return Err("Not little-endian");
    }

    let e_type = read_u16(elf_data, 16);
    let e_machine = read_u16(elf_data, 18);
    let e_entry = read_u64(elf_data, 24);
    let e_phoff = read_u64(elf_data, 32) as usize;
    let e_phentsize = read_u16(elf_data, 54) as usize;
    let e_phnum = read_u16(elf_data, 56) as usize;

    // Only ET_EXEC (2) is supported. ET_DYN/PIE would require applying dynamic
    // relocations, which this loader does not do — accepting it would silently
    // jump to a wrong entry point. The kernel is linked as a non-PIE ET_EXEC.
    if e_type != 2 {
        return Err("Not a non-PIE executable ELF (ET_EXEC required)");
    }

    // Validate the kernel's machine type matches the bootloader's target.
    #[cfg(target_arch = "x86_64")]
    const EXPECTED_MACHINE: u16 = 0x3E; // EM_X86_64
    #[cfg(target_arch = "riscv64")]
    const EXPECTED_MACHINE: u16 = 0xF3; // EM_RISCV
    if e_machine != EXPECTED_MACHINE {
        return Err("Architecture mismatch between bootloader and kernel");
    }
    // ELF64 program header is 56 bytes minimum
    if e_phentsize < 56 {
        return Err("Invalid program header size");
    }
    if e_phnum == 0 {
        return Err("No program headers");
    }

    // First pass: validate every PT_LOAD header and compute the physical span
    // [lowest p_paddr, highest p_paddr + p_memsz) so we can reserve it up front.
    let mut span_lo: u64 = u64::MAX;
    let mut span_hi: u64 = 0;
    for i in 0..e_phnum {
        let ph_offset = e_phoff
            .checked_add(i.checked_mul(e_phentsize).ok_or("phnum overflow")?)
            .ok_or("phoff overflow")?;
        if ph_offset.checked_add(e_phentsize).ok_or("phdr overflow")? > elf_data.len() {
            return Err("Program header out of bounds");
        }

        if read_u32(elf_data, ph_offset) != PT_LOAD {
            continue;
        }

        let p_offset = read_u64(elf_data, ph_offset + 8);
        let p_paddr = read_u64(elf_data, ph_offset + 24);
        let p_filesz = read_u64(elf_data, ph_offset + 32);
        let p_memsz = read_u64(elf_data, ph_offset + 40);

        if p_memsz < p_filesz {
            return Err("Segment memsz < filesz");
        }
        let end_in_file = (p_offset as usize)
            .checked_add(p_filesz as usize)
            .ok_or("segment offset overflow")?;
        if end_in_file > elf_data.len() {
            return Err("Segment data out of bounds");
        }
        let seg_end = p_paddr.checked_add(p_memsz).ok_or("segment paddr overflow")?;

        if p_memsz > 0 {
            span_lo = span_lo.min(p_paddr);
            span_hi = span_hi.max(seg_end);
        }
    }

    if span_hi == 0 {
        return Err("No loadable segments");
    }

    // Reserve the whole physical span (page aligned) as LOADER_DATA so UEFI will
    // not place anything there and the kernel later treats it as reserved.
    let base = span_lo & !(PAGE_SIZE - 1);
    let top = (span_hi + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let pages = ((top - base) / PAGE_SIZE) as usize;
    boot::allocate_pages(AllocateType::Address(base), MemoryType::LOADER_DATA, pages)
        .map_err(|_| "Failed to reserve kernel load region")?;

    // Second pass: copy each PT_LOAD segment into the reserved region.
    for i in 0..e_phnum {
        let ph_offset = e_phoff
            .checked_add(i.checked_mul(e_phentsize).ok_or("phnum overflow")?)
            .ok_or("phoff overflow")?;
        if ph_offset + e_phentsize > elf_data.len() {
            return Err("Program header out of bounds");
        }
        if read_u32(elf_data, ph_offset) != PT_LOAD {
            continue;
        }

        let p_offset = read_u64(elf_data, ph_offset + 8) as usize;
        let p_paddr = read_u64(elf_data, ph_offset + 24);
        let p_filesz = read_u64(elf_data, ph_offset + 32) as usize;
        let p_memsz = read_u64(elf_data, ph_offset + 40) as usize;

        // Validate segment fits within the reserved physical span.
        if p_paddr < base || p_paddr.saturating_add(p_memsz as u64) > top {
            return Err("Segment outside reserved region");
        }

        let dest = p_paddr as *mut u8;

        // copy_nonoverlapping handles unaligned source/dest correctly and is far
        // faster than a byte loop for a multi-hundred-KB kernel image.
        core::ptr::copy_nonoverlapping(elf_data.as_ptr().add(p_offset), dest, p_filesz);

        // Zero BSS (memsz > filesz).
        if p_memsz > p_filesz {
            core::ptr::write_bytes(dest.add(p_filesz), 0, p_memsz - p_filesz);
        }
    }

    Ok(e_entry)
}
