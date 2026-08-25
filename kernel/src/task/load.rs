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
use crate::mm::vmm::{PageFlags, Vmm, clone_high_half};
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
/// Canonical user stack ceiling.  The live per-process stack top is slid
/// down from here by [`aslr_stack_top`].
const USER_STACK_TOP: u64 = 0x0000_7FFF_0000_0000;

/// Maximum downward slide of the user stack top (256 MiB, 4 KiB granular).
///
/// Bounded so the stack can never climb into `CAP_SLOT_VA`
/// (0x7FFF_8000_0000 — a full GiB above this window's bottom) and so
/// `usermem::register`'s accounting sees a consistent ceiling.
const USER_STACK_ASLR_WINDOW: u64 = 256 * 1024 * 1024;

/// User images must stay below the lowest possible randomized stack guard.
/// Otherwise one valid stack slide could overlap an image page that was
/// already installed in the shared process root.
const USER_IMAGE_MAX: u64 = USER_STACK_TOP
    - USER_STACK_ASLR_WINDOW
    - USER_STACK_SIZE
    - 4096;
const USER_BOUNDARY: u64 = 0x0000_8000_0000_0000;

/// Randomized user stack top: `USER_STACK_TOP - rand[0, window)`, 4 KiB
/// aligned.  Uses the kernel CSPRNG (non-blocking; falls back to its entropy
/// source when unseeded), so a zero slide is possible but improbable.
fn aslr_stack_top() -> u64 {
    let pages = USER_STACK_ASLR_WINDOW / 4096;
    let off = (crate::random::random_u64() % pages) * 4096;
    USER_STACK_TOP - off
}

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
            .checked_add(
                (i as u64)
                    .checked_mul(e_phentsize as u64)
                    .ok_or("phnum overflow")?,
            )
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
        let end_in_file = p_offset
            .checked_add(p_filesz)
            .ok_or("segment offset overflow")?;
        if end_in_file > elf.len() as u64 {
            return Err("Segment data out of bounds");
        }
        let end = p_vaddr
            .checked_add(p_memsz)
            .ok_or("segment vaddr overflow")?;
        if p_memsz == 0 || p_vaddr < 4096 || end > USER_IMAGE_MAX || end > USER_BOUNDARY {
            return Err("Segment outside the permitted user image range");
        }
        if p_flags & PF_X != 0 && p_flags & 0x2 != 0 {
            return Err("Writable executable segment violates W^X");
        }

        segs.push(Segment {
            vaddr: p_vaddr,
            memsz: p_memsz,
            filesz: p_filesz,
            offset: p_offset,
            flags: p_flags,
        });
    }
    if segs.is_empty() {
        return Err("No loadable segments");
    }
    // A shared page may be reused by adjacent PT_LOADs, but it cannot safely
    // represent both executable and writable content.  Reject such images
    // instead of letting segment order silently decide the final PTE flags.
    for (i, a) in segs.iter().enumerate() {
        let a_start = a.vaddr & !0xFFF;
        let a_end = a
            .vaddr
            .checked_add(a.memsz)
            .and_then(|v| v.checked_add(0xFFF))
            .ok_or("segment page range overflow")?
            & !0xFFF;
        for b in segs.iter().skip(i + 1) {
            let b_start = b.vaddr & !0xFFF;
            let b_end = b
                .vaddr
                .checked_add(b.memsz)
                .and_then(|v| v.checked_add(0xFFF))
                .ok_or("segment page range overflow")?
                & !0xFFF;
            if a_start < b_end
                && b_start < a_end
                && (a.flags & PF_X != b.flags & PF_X
                    || a.flags & 0x2 != b.flags & 0x2)
            {
                return Err("Overlapping PT_LOAD pages have incompatible permissions");
            }
        }
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
        .checked_add(0xFFF)
        .ok_or("segment alignment overflow")?
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
            let src = elf.as_ptr().wrapping_add((seg.offset + seg_start) as usize);
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
/// Returns `(page_table_root, entry_point, user_stack_top, vm_idx)`.
pub fn create_process(
    elf: &[u8],
    alloc: &mut BitmapAllocator,
) -> Result<(u64, u64, u64, usize), &'static str> {
    let e_entry = read_u64(elf, 24);
    let segs = parse_segments(elf)?;

    let entry_ok = segs.iter().any(|seg| {
        seg.flags & PF_X != 0
            && e_entry >= seg.vaddr
            && e_entry < seg.vaddr.saturating_add(seg.memsz)
    });
    if !entry_ok {
        return Err("ELF entry point is not inside an executable segment");
    }

    // Clone the kernel higher half; the low half starts empty for the user.
    let root = clone_high_half(alloc, crate::task::kernel_root());
    let mut vmm = Vmm::from_root(root);

    for seg in &segs {
        if let Err(e) = map_segment(&mut vmm, alloc, elf, seg) {
            crate::mm::vmm::destroy_root(root, alloc);
            return Err(e);
        }
    }

    // 32 KiB user stack at the ASLR-slid top, guard page below unmapped.
    let stack_top = aslr_stack_top();
    let stack_base = stack_top - USER_STACK_SIZE;
    let mut va = stack_base;
    while va < stack_top {
        let Some(phys) = alloc.alloc() else {
            crate::mm::vmm::destroy_root(root, alloc);
            return Err("OOM mapping user stack");
        };
        vmm.map_4k(
            alloc,
            va,
            phys,
            PageFlags::USER | PageFlags::READ | PageFlags::WRITE,
        );
        unsafe {
            core::ptr::write_bytes(to_physmap(phys) as *mut u8, 0, 4096);
        }
        va += 4096;
    }

    // Page-aligned extent of the ELF image — the floor the program break grows
    // from. Registered with the eager user-memory table so brk/mmap/munmap and
    // `/proc/<pid>/mem` share one view.
    let mut image_floor = u64::MAX;
    let mut image_top = 0u64;
    for seg in &segs {
        let s = seg.vaddr & !0xFFF;
        let t = seg
            .vaddr
            .checked_add(seg.memsz)
            .ok_or("segment vaddr overflow")?
            .wrapping_add(0xFFF)
            & !0xFFF;
        if s < image_floor {
            image_floor = s;
        }
        if t > image_top {
            image_top = t;
        }
    }
    let stack_flags = PageFlags::USER | PageFlags::READ | PageFlags::WRITE;
    let vm = crate::mm::usermem::register(root, image_floor, image_top, stack_top, stack_flags);

    Ok((root, e_entry, stack_top, vm))
}

/// Encode a `struct{name: str}` method payload and invoke it on `path`.
/// Used to pre-create the INIT demo scratch tree. If the target already
/// exists (a path carries a provider error like ENOENT/EEXIST-adjacent), the
/// failure is ignored — the file just needs to be present before ring 3 runs.
fn precreate(path: &str, name: &str, payload: &mut Vec<u8>, out: &mut Vec<u8>) {
    payload.clear();
    if schema::encode_value(
        &Value::Struct(vec![Value::Str(String::from(name))]),
        &CREATE_INPUT,
        payload,
    )
    .is_ok()
    {
        let _ = unispace::write(path, payload, out);
    }
}

/// Load `\EFI\BEDROCK\INIT` from the ESP (mounted at `/B` in the unispace
/// registry), build its address space, and launch it into ring 3. No-op with a
/// serial notice when the file is absent.
///
/// Pre-creates the `/A/init` dir and `/A/init/test` file the demo writes into,
/// then starts INIT. When the task exits and parks back into idle, this
/// function resumes here, reads `/A/init/test` back through unispace and
/// prints its bytes to serial, and prints the task's stdout
/// (`/proc/<pid>/std/out`) — closing the write→read→exit→resume loop.
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

    let (root, entry, user_stack_top, vm) = match create_process(&elf, alloc) {
        Ok(x) => x,
        Err(e) => {
            log::warn!("[sched] failed to load INIT: {}", e);
            return;
        }
    };
    // Install manual fullcaps for INIT (no wildcard)
    {
        let caps = alloc::sync::Arc::new(crate::caps::full_caps_for_init());
        let caps_va = crate::mm::layout::pick_caps_va();
        if let Some(phys) = crate::task::install_caps(root, &caps, caps_va, alloc) {
            // Stash the Arc'd set; enter_userspace adopts it into the real Task.
            crate::task::stash_init_caps(caps, phys, caps_va);
        } else {
            // INIT without its capability mirror is not a usable process and
            // would otherwise run deny-all while leaking its address space.
            // Tear down the complete cloned root, including its root frame,
            // before returning to the kernel idle path.
            log::warn!("[sched] caps page alloc failed for INIT; rolling back");
            crate::mm::vmm::destroy_root(root, alloc);
            crate::mm::usermem::unregister(vm);
            return;
        }
        // Ensure root still has caps page mapped (install_caps mapped it)
        // No extra step: install_caps already mapped.
    }

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

    let pid = crate::task::enter_userspace(entry, user_stack_top, root, 0, vm, alloc);

    // The INIT task exited and parked into idle; we resumed here. It is not
    // yet reaped, so its `/proc` dir still exists — drain its stdout and
    // print it to serial.
    let mut stdout: Vec<u8> = Vec::new();
    let spath = alloc::format!("/proc/{}/std/out", pid);
    match crate::unispace::read(&spath, &mut stdout, usize::MAX) {
        Ok(()) => {
            SerialPort::puts("[sched] INIT stdout:\n");
            SerialPort::puts(core::str::from_utf8(&stdout).unwrap_or("<non-utf8>"));
            SerialPort::puts("\n");
        }
        Err(e) => log::warn!("[sched] read /proc/{}/std/out failed: {:?}", pid, e),
    }

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
