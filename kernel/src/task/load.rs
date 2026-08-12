//! User-mode process loader: ELF validation, address-space construction, and
//! the boot-time launch of `\EFI\BEDROCK\INIT` from the ESP.
//!
//! Phase 6. The loader parses an ELF64 ET_EXEC (linked at 0x400000), clones
//! the kernel higher half into a fresh root, maps every PT_LOAD segment with
//! USER permissions, sets up a 32 KiB user stack with a guard page, and hands
//! the new address space to `enter_userspace`. The INIT binary is fetched
//! through the unispace registry (`/B/...`, the VFS ESP mount) rather than the
//! VFS directly, so the boot path exercises the same namespace every other
//! component uses.
//!
//! Everything here is defensive: a malformed binary yields `Err`, never a
//! panic. The kernel aborts on panic (`panic = "abort"`), so bytes read from a
//! disk must only ever produce a `Result`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::serial::SerialPort;
use crate::mm::layout::to_physmap;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{clone_high_half, PageFlags, Vmm};
use crate::unispace;
use crate::unispace::provider::vfs::CREATE_INPUT;
use crate::unispace::schema::{self, Value};

/// ELF64 magic bytes.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// Program-header type for loadable segments.
const PT_LOAD: u32 = 1;

/// ELF segment flag: executable.
const PF_X: u32 = 0x1;

/// EM_X86_64 — the only machine the user loader accepts.
const EXPECTED_MACHINE: u16 = 0x3E;

/// User stack top; the stack grows down from here.
const USER_STACK_TOP: u64 = 0x0000_7FFF_0000_0000;

/// Size of the initial user stack (8 × 4 KiB). The page below it stays
/// unmapped as the guard page.
const USER_STACK_SIZE: u64 = 32 * 1024;

// ── Minimal ELF64 readers (little-endian) ─────────────────────────────
// Callers guarantee bounds before invoking these.

/// Read a little-endian u16 from `data` at `offset`.
fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

/// Read a little-endian u32 from `data` at `offset`.
fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Read a little-endian u64 from `data` at `offset`.
fn read_u64(data: &[u8], offset: usize) -> u64 {
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

/// ELF64 header size (the 64-byte ELF header).
const ELF_HEADER_SIZE: usize = 64;

/// One validated PT_LOAD segment, with the virtual range it must cover.
struct Segment {
    vaddr: u64,
    memsz: u64,
    filesz: u64,
    offset: u64,
    flags: u32,
}

/// Page flags for a segment, honoring W^X: executable segments map
/// USER|READ|EXECUTE (never WRITE), everything else USER|READ|WRITE.  The
/// loader writes segment bytes through the physmap (`to_physmap`), never the
/// user VA, so the non-writable .text mapping does not break image loading.
fn seg_page_flags(p_flags: u32) -> PageFlags {
    let mut f = PageFlags::USER | PageFlags::READ;
    if p_flags & PF_X != 0 {
        f |= PageFlags::EXECUTE;
    } else {
        f |= PageFlags::WRITE;
    }
    f
}

/// Parse and validate an ELF64 ET_EXEC image, returning its PT_LOAD segments.
fn parse_segments(elf: &[u8]) -> Result<Vec<Segment>, &'static str> {
    if elf.len() < ELF_HEADER_SIZE {
        return Err("ELF too small");
    }
    if elf[..4] != ELF_MAGIC {
        return Err("Invalid ELF magic");
    }
    if elf[4] != 2 {
        return Err("Not ELF64");
    }
    if elf[5] != 1 {
        return Err("Not little-endian");
    }

    let e_type = read_u16(elf, 16);
    let e_machine = read_u16(elf, 18);
    let e_phoff = read_u64(elf, 32);
    let e_phentsize = read_u16(elf, 54) as usize;
    let e_phnum = read_u16(elf, 56) as usize;

    if e_type != 2 {
        return Err("Not a non-PIE executable ELF (ET_EXEC required)");
    }
    if e_machine != EXPECTED_MACHINE {
        return Err("Architecture mismatch (expected EM_X86_64)");
    }
    if e_phentsize < 56 {
        return Err("Invalid program header size");
    }
    if e_phnum == 0 {
        return Err("No program headers");
    }

    let mut segs: Vec<Segment> = Vec::new();
    for i in 0..e_phnum {
        let ph_off = e_phoff
            .checked_add((i as u64).checked_mul(e_phentsize as u64).ok_or("phnum overflow")?)
            .ok_or("phoff overflow")?;
        let ph_off = ph_off as usize;
        if ph_off.checked_add(e_phentsize).ok_or("phdr overflow")? > elf.len() {
            return Err("Program header out of bounds");
        }

        if read_u32(elf, ph_off) != PT_LOAD {
            continue;
        }

        let p_offset = read_u64(elf, ph_off + 8);
        let p_vaddr = read_u64(elf, ph_off + 16);
        let p_filesz = read_u64(elf, ph_off + 32);
        let p_memsz = read_u64(elf, ph_off + 40);
        let p_flags = read_u32(elf, ph_off + 4);

        if p_memsz < p_filesz {
            return Err("Segment memsz < filesz");
        }
        let end_in_file = (p_offset as usize)
            .checked_add(p_filesz as usize)
            .ok_or("segment offset overflow")?;
        if end_in_file > elf.len() {
            return Err("Segment data out of bounds");
        }
        p_vaddr.checked_add(p_memsz).ok_or("segment vaddr overflow")?;

        segs.push(Segment { vaddr: p_vaddr, memsz: p_memsz, filesz: p_filesz, offset: p_offset, flags: p_flags });
    }
    if segs.is_empty() {
        return Err("No loadable segments");
    }
    Ok(segs)
}

/// Map the virtual range `[vstart, vend)` of a segment into `vmm`, copy the
/// `filesz` bytes that fall in each page, and zero the rest of each frame.
///
/// Frames are written through the physmap, never through the user VA — the
/// new root is not the active CR3, so a direct store at a user address would
/// fault.
fn map_segment(
    vmm: &mut Vmm,
    alloc: &mut BitmapAllocator,
    elf: &[u8],
    seg: &Segment,
) -> Result<(), &'static str> {
    let vstart = seg.vaddr & !0xFFF;
    let vend = seg
        .vaddr
        .checked_add(seg.memsz)
        .ok_or("segment vaddr overflow")?
        .wrapping_add(0xFFF)
        & !0xFFF;

    let mut va = vstart;
    while va < vend {
        // ELF permits load segments to abut inside a page, so a later PT_LOAD
        // can cover a page an earlier one already mapped (e.g. .rodata ending
        // in the same 4 KiB as the start of .got). Reuse the existing mapping
        // and copy this segment's bytes into it instead of double-mapping.
        // Zeroing is skipped on reuse: the segment that first mapped the page
        // already cleared it.
        let phys = match vmm.translate(va) {
            Some(p) => p,
            None => {
                let p = alloc.alloc().ok_or("OOM mapping ELF segment")?;
                vmm.map_4k(alloc, va, p, seg_page_flags(seg.flags));
                let dst = to_physmap(p) as *mut u8;
                unsafe {
                    core::ptr::write_bytes(dst, 0, 4096);
                }
                p
            }
        };

        let dst = to_physmap(phys) as *mut u8;

        // Bytes of the segment already covered by prior pages, plus the
        // unaligned start offset on the first page.
        let rel = va - vstart;
        let page_offset = if rel == 0 { seg.vaddr & 0xFFF } else { 0 };
        let seg_start = rel + page_offset;

        if seg_start < seg.filesz {
            let n = core::cmp::min(seg.filesz - seg_start, 4096 - page_offset);
            let src = elf
                .as_ptr()
                .wrapping_add((seg.offset + seg_start) as usize);
            unsafe {
                core::ptr::copy_nonoverlapping(src, dst.add(page_offset as usize), n as usize);
            }
        }

        va += 4096;
    }
    Ok(())
}

/// Build a user address space from an ELF image.
///
/// Clones the kernel higher half into a fresh root, maps every PT_LOAD
/// segment at its `p_vaddr` with USER permissions, and places a 32 KiB user
/// stack at the canonical user ceiling with an unmapped guard page below.
///
/// Returns `(page_table_root, entry_point, user_stack_top)`.
pub fn create_process(
    elf: &[u8],
    alloc: &mut BitmapAllocator,
) -> Result<(u64, u64, u64), &'static str> {
    let e_entry = read_u64(elf, 24);
    let segs = parse_segments(elf)?;

    // Clone the kernel higher half; the low half starts empty for the user.
    let root = clone_high_half(alloc, crate::task::kernel_root());
    let mut vmm = Vmm::from_root(root);

    for seg in &segs {
        map_segment(&mut vmm, alloc, elf, seg)?;
    }

    // 32 KiB user stack at the canonical ceiling, guard page below unmapped.
    let stack_base = USER_STACK_TOP - USER_STACK_SIZE;
    let mut va = stack_base;
    while va < USER_STACK_TOP {
        let phys = alloc.alloc().ok_or("OOM mapping user stack")?;
        vmm.map_4k(alloc, va, phys, PageFlags::USER | PageFlags::READ | PageFlags::WRITE);
        unsafe {
            core::ptr::write_bytes(to_physmap(phys) as *mut u8, 0, 4096);
        }
        va += 4096;
    }

    Ok((root, e_entry, USER_STACK_TOP))
}

/// Encode a `struct{name: str}` method payload and invoke it on `path`.
/// Used to pre-create the INIT demo scratch tree. If the target already
/// exists (a path carries a provider error like ENOENT/EEXIST-adjacent), the
/// failure is ignored — the file just needs to be present before ring 3 runs.
fn precreate(path: &str, name: &str, payload: &mut Vec<u8>, out: &mut Vec<u8>) {
    payload.clear();
    if schema::encode_value(&Value::Struct(vec![Value::Str(String::from(name))]), &CREATE_INPUT, payload).is_ok() {
        let _ = unispace::write(path, payload, out);
    }
}

/// Load `\EFI\BEDROCK\INIT` from the ESP (mounted at `/B` in the unispace
/// registry), build its address space, and launch it into ring 3. No-op with a
/// serial notice when the file is absent.
///
/// Pre-creates the `/A/init` dir and `/A/init/test` file the demo writes into,
/// then starts INIT. When the task exits and parks back into idle, this
/// function resumes here, reads `/A/init/test` back through unispace, and
/// prints its bytes to serial — closing the write→read→exit→resume loop.
pub fn load_init_from_esp(alloc: &mut BitmapAllocator) {
    let mut elf: Vec<u8> = Vec::new();
    if let Err(_) = unispace::read("/B/EFI/BEDROCK/INIT", &mut elf, usize::MAX) {
        log::info!("[sched] INIT not found, skipping user-mode launch");
        return;
    }

    // Scratch tree the INIT demo exercises: /A/init/test.
    let mut payload = Vec::new();
    let mut out = Vec::new();
    precreate("/A:mkdir", "init", &mut payload, &mut out);
    precreate("/A/init:create", "test", &mut payload, &mut out);

    let (root, entry, user_stack_top) = match create_process(&elf, alloc) {
        Ok(x) => x,
        Err(e) => {
            log::warn!("[sched] failed to load INIT: {}", e);
            return;
        }
    };

    SerialPort::puts("[sched] init at 0x");
    SerialPort::put_hex(entry);
    SerialPort::puts(" root 0x");
    SerialPort::put_hex(root);
    SerialPort::puts(" stack 0x");
    SerialPort::put_hex(user_stack_top);
    SerialPort::puts("\n");

    // Prove the pre-swapgs handoff: with `set_user_gs(0)` in effect the kernel
    // must run with GS.base = PerCpu and KERNEL_GS_BASE = 0. `enter_userspace`
    // calls the same helper again (idempotent) before the final swapgs.
    crate::arch::x86_64::syscall::set_user_gs(0);
    SerialPort::puts("[sched] GS.base=0x");
    SerialPort::put_hex(x86_64::registers::model_specific::GsBase::read().as_u64());
    SerialPort::puts(" KERNEL_GS_BASE=0x");
    SerialPort::put_hex(x86_64::registers::model_specific::KernelGsBase::read().as_u64());
    SerialPort::puts(" frame.CS=0x2B\n");

    // Phase 8: the user address space and syscall entry stub now exist, so
    // enable SYSCALL/SYSRET (EFER.SCE + STAR/LSTAR/SFMASK). With SCE clear the
    // first `syscall` from ring 3 raises #UD; with a bogus STAR/LSTAR it would
    // be a #GP. `setup_syscall_msrs` is deferred to here because programming
    // LSTAR before the stub exists would land any stray syscall in garbage.
    crate::arch::x86_64::syscall::setup_syscall_msrs(
        crate::arch::x86_64::syscall::syscall_entry_addr(),
    );
    SerialPort::puts("[sched] LSTAR=0x");
    SerialPort::put_hex(crate::arch::x86_64::syscall::syscall_entry_addr());
    SerialPort::puts(" STAR=0x");
    SerialPort::put_hex(0x001B_0018_0000_0000u64);
    SerialPort::puts(" SCE=1\n");

    crate::task::enter_userspace(entry, user_stack_top, root, 0, alloc);

    // The INIT task exited and parked into idle; we resumed here. Read back
    // what it wrote to prove the write→read→exit→resume cycle end-to-end.
    let mut check: Vec<u8> = Vec::new();
    match unispace::read("/A/init/test", &mut check, usize::MAX) {
        Ok(()) => {
            SerialPort::puts("[sched] /A/init/test = ");
            SerialPort::puts(core::str::from_utf8(&check).unwrap_or("<non-utf8>"));
            SerialPort::puts("\n");
        }
        Err(e) => log::warn!("[sched] readback /A/init/test failed: {:?}", e),
    }
}
