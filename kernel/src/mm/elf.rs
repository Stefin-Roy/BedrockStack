//! Minimal ELF64 loader.
//!
//! Parses a static-pie or fixed-address ELF64 executable and maps its
//! PT_LOAD segments into a user address space (a `Vmm` with empty low
//! half). Returns the entry point virtual address.
//!
//! All ELF fields are read through byte-slice helpers, so malformed or
//! unaligned input can never produce an unaligned reference (the boot
//! loader's `boot::elf` uses the same approach).

extern crate alloc;

use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};

/// ELF loading errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Not an ELF file (magic mismatch).
    NotElf,
    /// Not a 64-bit ELF.
    Not64Bit,
    /// Not little-endian.
    NotLittleEndian,
    /// Not an executable (ET_EXEC).
    NotExecutable,
    /// Not x86_64 machine type.
    WrongMachine,
    /// Invalid program header.
    InvalidPhdr,
    /// Out of memory.
    OutOfMemory,
    /// Segment exceeds the user address space.
    SegmentTooLarge,
}

const ELFMAG: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

/// Top of the x86_64 low (user) canonical half. Segments and the entry point
/// must stay below this.
const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Read a little-endian u16 from a byte slice at the given offset.
fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(u16::from_le_bytes([data[offset], data[offset + 1]]))
}

/// Read a little-endian u32 from a byte slice at the given offset.
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > data.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

/// Read a little-endian u64 from a byte slice at the given offset.
fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    if offset + 8 > data.len() {
        return None;
    }
    Some(u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]))
}

/// Load an ELF64 executable into the given user address space.
///
/// # Safety
/// `data` must be a valid ELF64 executable. `vmm` must be a user address
/// space (low half empty). The caller must ensure the data slice outlives
/// the mapping operation.
pub fn load_elf(
    data: &[u8],
    vmm: &mut Vmm,
    alloc: &mut BitmapAllocator,
) -> Result<u64, ElfError> {
    if data.len() < 64 {
        return Err(ElfError::NotElf);
    }

    // Validate ELF identity.
    if data[..4] != ELFMAG {
        return Err(ElfError::NotElf);
    }
    if data[4] != ELFCLASS64 {
        return Err(ElfError::Not64Bit);
    }
    if data[5] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }
    if read_u16(data, 16).ok_or(ElfError::InvalidPhdr)? != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if read_u16(data, 18).ok_or(ElfError::InvalidPhdr)? != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }

    let entry = read_u64(data, 24).ok_or(ElfError::InvalidPhdr)?;
    let phoff = read_u64(data, 32).ok_or(ElfError::InvalidPhdr)?;
    let phentsize = read_u16(data, 54).ok_or(ElfError::InvalidPhdr)? as usize;
    let phnum = read_u16(data, 56).ok_or(ElfError::InvalidPhdr)? as usize;

    if entry >= USER_LIMIT {
        return Err(ElfError::SegmentTooLarge);
    }
    if phentsize < 56 {
        return Err(ElfError::InvalidPhdr);
    }
    if phnum == 0 {
        return Err(ElfError::InvalidPhdr);
    }

    // Walk program headers, mapping PT_LOAD segments.
    for i in 0..phnum {
        let off = phoff
            .checked_add((i as u64).checked_mul(phentsize as u64).ok_or(ElfError::InvalidPhdr)?)
            .ok_or(ElfError::InvalidPhdr)? as usize;
        if off.checked_add(phentsize).ok_or(ElfError::InvalidPhdr)? > data.len() {
            return Err(ElfError::InvalidPhdr);
        }

        if read_u32(data, off).ok_or(ElfError::InvalidPhdr)? != PT_LOAD {
            continue;
        }

        let file_off = read_u64(data, off + 8).ok_or(ElfError::InvalidPhdr)?;
        let vaddr = read_u64(data, off + 16).ok_or(ElfError::InvalidPhdr)?;
        let filesz = read_u64(data, off + 32).ok_or(ElfError::InvalidPhdr)?;
        let memsz = read_u64(data, off + 40).ok_or(ElfError::InvalidPhdr)?;

        if memsz == 0 {
            continue;
        }
        if memsz < filesz {
            return Err(ElfError::InvalidPhdr);
        }
        // Segment must live in the user (low) half and not wrap.
        let seg_end = vaddr.checked_add(memsz).ok_or(ElfError::SegmentTooLarge)?;
        if seg_end > USER_LIMIT {
            return Err(ElfError::SegmentTooLarge);
        }

        let p_flags = read_u32(data, off + 4).ok_or(ElfError::InvalidPhdr)?;
        let write = (p_flags & 2) != 0;
        let execute = (p_flags & 1) != 0;

        let mut page_flags = PageFlags::READ | PageFlags::USER;
        if write {
            page_flags |= PageFlags::WRITE;
        }
        if execute {
            page_flags |= PageFlags::EXECUTE;
        }

        let start_page = vaddr & !0xFFF;
        let end_page = (seg_end + 0xFFF) & !0xFFF;

        // Map pages for this segment, placing file data at the exact offset
        // that corresponds to each virtual address (handles unaligned vaddrs).
        let mut page_addr = start_page;
        while page_addr < end_page {
            let frame = alloc.alloc().ok_or(ElfError::OutOfMemory)?;
            let frame_va = crate::mm::layout::to_physmap(frame);

            // Zero the frame (also covers the BSS tail: memsz > filesz).
            unsafe {
                core::ptr::write_bytes(frame_va as *mut u8, 0, 4096);
            }

            // Bytes of this page that come from the file: the byte at vaddr
            // maps to file offset `file_off`, so a page that starts at
            // `page_addr` holds file data starting at segment position
            // `max(0, page_addr - vaddr)`, placed `max(0, vaddr - page_addr)`
            // bytes into the frame (handles unaligned vaddrs).
            let dest_off = if vaddr > page_addr { (vaddr - page_addr) as usize } else { 0 };
            let seg_rel = if page_addr > vaddr { page_addr - vaddr } else { 0 };
            if seg_rel < filesz {
                let avail_in_page = 4096 - dest_off;
                let file_remaining = filesz - seg_rel;
                let to_copy = core::cmp::min(file_remaining, avail_in_page as u64) as usize;
                if to_copy > 0 {
                    let src_off =
                        file_off.checked_add(seg_rel).ok_or(ElfError::InvalidPhdr)? as usize;
                    if src_off.checked_add(to_copy).ok_or(ElfError::InvalidPhdr)? > data.len() {
                        return Err(ElfError::InvalidPhdr);
                    }
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            data.as_ptr().add(src_off),
                            (frame_va + dest_off as u64) as *mut u8,
                            to_copy,
                        );
                    }
                }
            }

            vmm.map_4k(alloc, page_addr, frame, page_flags);
            page_addr += 4096;
        }
    }

    Ok(entry)
}
