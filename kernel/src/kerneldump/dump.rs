//! Fault diagnostic dump — register, stack, code & page-table output.
//!
//! Replaces bare `fault_halt` handlers.  Uses the tiny disassembler in
//! `super::disasm`.  Self-contained with no dependency on scheduler /
//! VCPU / service infra.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::structures::idt::InterruptStackFrame;

use crate::drivers::serial::SerialPort;
use crate::mm::vmm::KERNEL_VMA_BASE;

// ── x86-64 PTE flag constants ───────────────────────────────────────

const PTE_PRESENT:  u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER:     u64 = 1 << 2;
const PTE_NO_EXEC:  u64 = 1 << 63;

/// Maximum physical address we trust for identity-mapped access.
const MAX_PHYS: u64 = 4 * 1024 * 1024 * 1024;

// ── Re-entrancy guard ───────────────────────────────────────────────

static DUMP_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

// ── Exception name ──────────────────────────────────────────────────

fn exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "#DE",  1 => "#DB",  2 => "#NMI", 3 => "#BP",
        4 => "#OF",  5 => "#BR",  6 => "#UD",  7 => "#NM",
        8 => "#DF",  9 => "#COP",10 => "#TS", 11 => "#NP",
        12 => "#SS", 13 => "#GP", 14 => "#PF", 16 => "#MF",
        17 => "#AC", 18 => "#MC", 19 => "#XM", 20 => "#VE",
        _ => "??",
    }
}

// ── Null writer (pre-scan instruction lengths without output) ───────

struct NullWrite;
impl Write for NullWrite {
    fn write_str(&mut self, _: &str) -> core::fmt::Result { Ok(()) }
}

// ── Safe memory probes ──────────────────────────────────────────────

unsafe fn read_volatile_u64(ptr: *const u64) -> Option<u64> {
    if ptr.is_null() { return None; }
    // SAFETY: caller guarantees ptr is valid for read.
    Some(unsafe { core::ptr::read_volatile(ptr) })
}

fn probe_read_quad(cr3: u64, addr: u64) -> Option<u64> {
    // Canonical check.
    let ext = (addr as i64) >> 47;
    if ext != 0 && ext != -1 {
        return None;
    }

    let pml4_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    if pml4_phys >= MAX_PHYS { return None; }

    unsafe {
        let pml4_virt = KERNEL_VMA_BASE + pml4_phys;
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pml4_entry = read_volatile_u64((pml4_virt + (pml4_idx as u64) * 8) as *const u64)?;
        if pml4_entry & PTE_PRESENT == 0 { return None; }

        let pdp_phys = pml4_entry & 0x000F_FFFF_FFFF_F000;
        if pdp_phys >= MAX_PHYS { return None; }
        let pdp_virt = KERNEL_VMA_BASE + pdp_phys;
        let pdp_idx = ((addr >> 30) & 0x1FF) as usize;
        let pdp_entry = read_volatile_u64((pdp_virt + (pdp_idx as u64) * 8) as *const u64)?;
        if pdp_entry & PTE_PRESENT == 0 { return None; }
        if pdp_entry & (1 << 7) != 0 {
            let phys = (pdp_entry & 0x000F_FFC0_0000_0000) | (addr & 0x3FFF_FFFF);
            if phys >= MAX_PHYS { return None; }
            return Some(core::ptr::read_volatile((KERNEL_VMA_BASE + phys) as *const u64));
        }

        let pd_phys = pdp_entry & 0x000F_FFFF_FFFF_F000;
        if pd_phys >= MAX_PHYS { return None; }
        let pd_virt = KERNEL_VMA_BASE + pd_phys;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        let pd_entry = read_volatile_u64((pd_virt + (pd_idx as u64) * 8) as *const u64)?;
        if pd_entry & PTE_PRESENT == 0 { return None; }
        if pd_entry & (1 << 7) != 0 {
            let phys = (pd_entry & 0x000F_FFFF_FE00_0000) | (addr & 0x1F_FFFF);
            if phys >= MAX_PHYS { return None; }
            return Some(core::ptr::read_volatile((KERNEL_VMA_BASE + phys) as *const u64));
        }

        let pt_phys = pd_entry & 0x000F_FFFF_FFFF_F000;
        if pt_phys >= MAX_PHYS { return None; }
        let pt_virt = KERNEL_VMA_BASE + pt_phys;
        let pt_idx = ((addr >> 12) & 0x1FF) as usize;
        let pte = read_volatile_u64((pt_virt + (pt_idx as u64) * 8) as *const u64)?;
        if pte & PTE_PRESENT == 0 { return None; }

        let phys = (pte & 0x000F_FFFF_FFFF_F000) | (addr & 0xFFF);
        if phys >= MAX_PHYS { return None; }
        Some(core::ptr::read_volatile((KERNEL_VMA_BASE + phys) as *const u64))
    }
}

// ── Stack dump ──────────────────────────────────────────────────────

fn dump_fault_stack(w: &mut impl Write, rsp: u64, cr3: u64) {
    let _ = writeln!(w, "--- Stack Dump (up to 32 quadwords from RSP) ---");

    for row in 0..8 {
        let base = rsp.wrapping_add(row as u64 * 32);
        let mut any_valid = false;
        let _ = write!(w, "  {:#018x}:", base);
        for col in 0..4 {
            let addr = base.wrapping_add(col as u64 * 8);
            match probe_read_quad(cr3, addr) {
                Some(val) => {
                    let _ = write!(w, "  {:#018x}", val);
                    any_valid = true;
                }
                None => { let _ = write!(w, "  ________________"); }
            }
        }
        let _ = writeln!(w);
        if !any_valid { break; }
    }
}

// ── Code disassembly ────────────────────────────────────────────────

fn dump_code_bytes(w: &mut impl Write, rip: u64, cr3: u64) {
    let _ = writeln!(w, "--- Code (instructions around RIP) ---");

    let start_addr = rip.saturating_sub(32) & !7;
    let mut buf = [0u8; 64];
    let mut valid = 0usize;
    for i in 0..8 {
        let addr = start_addr.wrapping_add(i as u64 * 8);
        if let Some(val) = probe_read_quad(cr3, addr) {
            buf[valid..][..8].copy_from_slice(&val.to_le_bytes());
            valid += 8;
        } else {
            valid += 8;
        }
    }

    if valid == 0 {
        let _ = writeln!(w, "  (no code readable)");
        return;
    }

    // Pre-scan: identify instruction boundaries
    let mut insn_offsets = [0usize; 64];
    let mut num_insns = 0;
    let mut rip_insn_idx = None;

    let mut offset = 0;
    while offset < valid && num_insns < 64 {
        let addr = start_addr.wrapping_add(offset as u64);
        let len = super::disasm::disasm_one(addr, &buf[offset..], &mut NullWrite).unwrap_or(0);
        let len = if len == 0 { 1 } else { len };

        insn_offsets[num_insns] = offset;
        if rip >= addr && rip < addr.wrapping_add(len as u64) {
            rip_insn_idx = Some(num_insns);
        }

        offset += len;
        num_insns += 1;
    }

    let rip_idx = rip_insn_idx.unwrap_or(0);
    let start_idx = rip_idx.saturating_sub(4);
    let end_idx = (rip_idx + 5).min(num_insns);

    for i in start_idx..end_idx {
        let offset = insn_offsets[i];
        let addr = start_addr.wrapping_add(offset as u64);

        let _ = write!(w, "  {:#018x}:", addr);

        let len = super::disasm::disasm_one(addr, &buf[offset..], &mut NullWrite).unwrap_or(0);
        let len = if len == 0 { 1 } else { len };

        for j in 0..len {
            let _ = write!(w, " {:02x}", buf[offset + j]);
        }
        let pad_len = (25usize).saturating_sub(len * 3);
        for _ in 0..pad_len { let _ = write!(w, " "); }

        super::disasm::disasm_one(addr, &buf[offset..], w);

        if i == rip_idx {
            let _ = write!(w, "  <-- RIP");
        }
        let _ = writeln!(w);
    }
}

// ── Error-code decoder ──────────────────────────────────────────────

fn dump_error_code(w: &mut impl Write, vector: u8, code: u64) {
    match vector {
        14 => {
            let p   = (code >> 0) & 1;
            let wr  = (code >> 1) & 1;
            let us  = (code >> 2) & 1;
            let rsv = (code >> 3) & 1;
            let id  = (code >> 4) & 1;
            let pk  = (code >> 5) & 1;
            let ss  = (code >> 6) & 1;
            let _sgx = (code >> 15) & 1;

            let _ = writeln!(w, "--- Page Fault Error Code ({:#x}) ---", code);
            let _ = writeln!(w, "  P    = {}  {}", p,    if p   != 0 { "Protection violation"     } else { "Not present"            });
            let _ = writeln!(w, "  W/R  = {}  {}", wr,   if wr  != 0 { "Write access"              } else { "Read access"            });
            let _ = writeln!(w, "  U/S  = {}  {}", us,   if us  != 0 { "User mode"                 } else { "Supervisor mode"        });
            let _ = writeln!(w, "  RSVD = {}", rsv);
            let _ = writeln!(w, "  I/D  = {}  {}", id,   if id  != 0 { "Instruction fetch"         } else { "Data access"            });
            let _ = writeln!(w, "  PK   = {}", pk);
            let _ = writeln!(w, "  SS   = {}", ss);
        }
        10 | 11 | 12 | 13 => {
            let _ = writeln!(w, "Error code: {:#x}", code);
            let ext   = (code >> 0) & 1;
            let table = (code >> 1) & 3;
            let index = (code >> 3) & 0x1FFF;
            let table_name = ["GDT", "IDT", "LDT", "IDT"][table as usize];
            let _ = writeln!(w, "  External : {}", if ext != 0 { "Yes (event sourced externally)" } else { "No" });
            let _ = writeln!(w, "  Table    : {} ({})", table_name, match table { 0 => "GDT", 1 => "IDT", 2 => "LDT", _ => "IDT" });
            let _ = writeln!(w, "  Selector : {:#05x} (index {})", index << 3, index);
        }
        _ => {
            let _ = writeln!(w, "Error code: {:#x}", code);
        }
    }
}

// ── CPUID identification ────────────────────────────────────────────

fn write_cpuid_info(w: &mut impl Write) {
    let mut vendor = [0u8; 12];
    let mut eax_1: u32 = 0;
    let mut ecx_1: u32 = 0;
    let mut edx_1: u32 = 0;
    let mut edx_8: u32 = 0;
    let mut _ecx_7: u32 = 0;
    let mut ebx_7: u32 = 0;

    unsafe {
        asm!("push rbx", "mov eax, 0", "cpuid", "mov [{v}], ebx", "mov [{v}+4], edx", "mov [{v}+8], ecx", "pop rbx",
             v = in(reg) vendor.as_mut_ptr(),
             out("eax") _, out("ecx") _, out("edx") _);

        asm!("push rbx", "mov eax, 1", "cpuid", "mov {0:e}, eax", "mov {1:e}, ecx", "mov {2:e}, edx", "pop rbx",
             out(reg) eax_1, out(reg) ecx_1, out(reg) edx_1);

        asm!("push rbx", "mov eax, 7", "xor ecx, ecx", "cpuid", "mov {0:e}, ebx", "mov {1:e}, ecx", "pop rbx",
             out(reg) ebx_7, out(reg) _ecx_7);

        asm!("push rbx", "mov eax, 0x80000001", "cpuid", "mov {0:e}, edx", "pop rbx",
             out(reg) edx_8,
             out("eax") _, out("ecx") _);
    }

    let stepping = eax_1 & 0xF;
    let model   = ((eax_1 >> 4) & 0xF) | ((eax_1 >> 12) & 0xF0);
    let family  = ((eax_1 >> 8) & 0xF) + if (eax_1 >> 8) & 0xF == 0xF { (eax_1 >> 20) & 0xFF } else { 0 };
    let v = core::str::from_utf8(&vendor).unwrap_or("unknown");

    let _ = writeln!(w, "CPUID: {}  Family {}  Model {}  Stepping {}", v, family, model, stepping);

    let _ = write!(w, "Features:");
    macro_rules! feat { ($cond:expr, $name:expr) => { if $cond { let _ = write!(w, " {}", $name); } }; }
    feat!((edx_1 >> 25) & 1 != 0, "sse");
    feat!((edx_1 >> 26) & 1 != 0, "sse2");
    feat!((ecx_1 >> 0)  & 1 != 0, "sse3");
    feat!((ecx_1 >> 9)  & 1 != 0, "ssse3");
    feat!((ecx_1 >> 19) & 1 != 0, "sse4.1");
    feat!((ecx_1 >> 20) & 1 != 0, "sse4.2");
    feat!((ecx_1 >> 28) & 1 != 0, "avx");
    feat!((ecx_1 >> 26) & 1 != 0, "xsave");
    feat!((edx_8 >> 11) & 1 != 0, "syscall");
    feat!((edx_8 >> 20) & 1 != 0, "nx");
    feat!((edx_8 >> 27) & 1 != 0, "rdtscp");
    feat!((ebx_7 >> 7)  & 1 != 0, "smep");
    feat!((ebx_7 >> 20) & 1 != 0, "smap");
    feat!((ebx_7 >> 0)  & 1 != 0, "fsgsbase");
    let _ = writeln!(w);
}

// ── Important MSRs ──────────────────────────────────────────────────

unsafe fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: caller guarantees MSR number is valid.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi); }
    ((hi as u64) << 32) | (lo as u64)
}

fn dump_msrs(w: &mut impl Write) {
    let _ = writeln!(w, "--- Important MSRs ---");
    unsafe {
        let efer = read_msr(0xC0000080);
        let star = read_msr(0xC0000081);
        let lstar = read_msr(0xC0000082);
        let cstar = read_msr(0xC0000083);
        let fmask = read_msr(0xC0000084);
        let fs_base = read_msr(0xC0000100);
        let gs_base = read_msr(0xC0000101);
        let kernel_gs_base = read_msr(0xC0000102);

        let _ = writeln!(w, "EFER        = {:#018x}", efer);
        let _ = writeln!(w, "  SCE ={}   System Call Extensions", (efer >> 0) & 1);
        let _ = writeln!(w, "  LME ={}   Long Mode Enable", (efer >> 8) & 1);
        let _ = writeln!(w, "  LMA ={}   Long Mode Active", (efer >> 10) & 1);
        let _ = writeln!(w, "  NXE ={}   No-Execute Enable", (efer >> 11) & 1);
        let _ = writeln!(w, "  SVME={}   SVM Enable", (efer >> 12) & 1);
        let _ = writeln!(w, "  LMSLE={}  Long Mode Segment Limit", (efer >> 13) & 1);
        let _ = writeln!(w, "  FFXSR={}  Fast FXSAVE/FXRSTOR", (efer >> 14) & 1);
        let _ = writeln!(w, "  TCE ={}   Translation Cache Extension", (efer >> 15) & 1);
        let _ = writeln!(w, "STAR        = {:#018x}", star);
        let _ = writeln!(w, "LSTAR       = {:#018x}", lstar);
        let _ = writeln!(w, "CSTAR       = {:#018x}", cstar);
        let _ = writeln!(w, "FMASK       = {:#018x}  (IF={})", fmask, if fmask & 0x200 != 0 { "masked" } else { "unmasked" });
        let _ = writeln!(w, "FS_BASE     = {:#018x}", fs_base);
        let _ = writeln!(w, "GS_BASE     = {:#018x}", gs_base);
        let _ = writeln!(w, "KERNEL_GS_BASE = {:#018x}", kernel_gs_base);
    }
}

// ── CR0 / CR4 flag decoders ───────────────────────────────────────

fn write_cr0_flags(w: &mut impl Write, cr0: u64) {
    let flags = [
        ("PE    ",  0, "Protected mode"),
        ("MP    ",  1, "Monitor co-processor"),
        ("EM    ",  2, "Emulation"),
        ("TS    ",  3, "Task switched"),
        ("NE    ",  5, "Numeric error"),
        ("WP    ", 16, "Write protect"),
        ("AM    ", 18, "Alignment mask"),
        ("NW    ", 29, "Not write-through"),
        ("CD    ", 30, "Cache disable"),
        ("PG    ", 31, "Paging"),
    ];
    for &(name, bit, desc) in &flags {
        let v = (cr0 >> bit) & 1;
        let _ = writeln!(w, "      {} = {}  {}", name, v, if v != 0 { desc } else { "" });
    }
}

fn write_cr4_flags(w: &mut impl Write, cr4: u64) {
    let flags = [
        ("VME        ",  0, "VM Extensions"),
        ("PVI        ",  1, "Protected-mode VM"),
        ("TSD        ",  2, "Time-stamp disable"),
        ("DE         ",  3, "Debugging extensions"),
        ("PSE        ",  4, "Page size extensions"),
        ("PAE        ",  5, "Physical address extension"),
        ("MCE        ",  6, "Machine check enable"),
        ("PGE        ",  7, "Page global enable"),
        ("PCE        ",  8, "Performance counter enable"),
        ("OSFXSR     ",  9, "FXSAVE/FXRSTOR"),
        ("OSXMMEXCPT ", 10, "SSE unmasked exceptions"),
        ("UMIP       ", 11, "UMIP"),
        ("LA57       ", 12, "57-bit VA"),
        ("VMXE       ", 13, "VMX enable"),
        ("SMXE       ", 14, "SMX enable"),
        ("FSGSBASE   ", 16, "FS/GS base access"),
        ("PCIDE      ", 17, "PCID enable"),
        ("OSXSAVE    ", 18, "XSAVE"),
        ("SMEP       ", 20, "SMEP"),
        ("SMAP       ", 21, "SMAP"),
        ("PKE        ", 22, "Protection key"),
        ("CET        ", 23, "CET"),
        ("PKS        ", 24, "Protection key supervisor"),
    ];
    for &(name, bit, desc) in &flags {
        let v = (cr4 >> bit) & 1;
        let _ = writeln!(w, "      {} = {}  {}", name, v, if v != 0 { desc } else { "" });
    }
}

// ── RFLAGS decoder ─────────────────────────────────────────────────

fn write_rflags(w: &mut impl Write, rflags: u64) {
    let flags = [
        ("CF",  0, "Carry"),
        ("PF",  2, "Parity"),
        ("AF",  4, "Adjust"),
        ("ZF",  6, "Zero"),
        ("SF",  7, "Sign"),
        ("TF",  8, "Trap (single-step)"),
        ("IF",  9, "Interrupt enable"),
        ("DF", 10, "Direction"),
        ("OF", 11, "Overflow"),
        ("NT", 14, "Nested task"),
        ("RF", 16, "Resume"),
        ("VM", 17, "Virtual-8086 mode"),
        ("AC", 18, "Alignment check"),
        ("VIF", 19, "Virtual interrupt"),
        ("VIP", 20, "Virtual interrupt pending"),
        ("ID", 21, "ID flag"),
    ];
    let iopl = (rflags >> 12) & 3;
    for &(name, bit, desc) in &flags {
        if bit == 12 { continue; }
        let v = (rflags >> bit) & 1;
        let _ = writeln!(w, "      {:4} = {}  {}", name, v, if v != 0 { desc } else { "" });
    }
    let _ = writeln!(w, "      IOPL = {}  I/O privilege level {}", iopl, iopl);
}

// ── Page-table walk ────────────────────────────────────────────────

fn write_pte_entry(w: &mut impl Write, label: &str, idx: usize, entry: u64) {
    let _ = write!(w, "  {}[{}] = {:#018x}", label, idx, entry);
    if entry & PTE_PRESENT  != 0 { let _ = write!(w, " P"); } else { let _ = write!(w, " ."); }
    if entry & PTE_WRITABLE != 0 { let _ = write!(w, " W"); } else { let _ = write!(w, " ."); }
    if entry & PTE_USER     != 0 { let _ = write!(w, " U"); } else { let _ = write!(w, " ."); }
    if entry & (1 << 5)     != 0 { let _ = write!(w, " A"); } else { let _ = write!(w, " ."); }
    if entry & (1 << 6)     != 0 { let _ = write!(w, " D"); } else { let _ = write!(w, " ."); }
    if entry & (1 << 8)     != 0 { let _ = write!(w, " G"); }
    if entry & (1 << 7)     != 0 { let _ = write!(w, " PS"); }
    if entry & PTE_NO_EXEC  != 0 { let _ = write!(w, " NX"); } else { let _ = write!(w, " X"); }
    let phys = entry & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "   phys={:#x}", phys);
}

fn dump_page_walk(w: &mut impl Write, cr3: u64, vaddr: u64) {
    let _ = writeln!(w, "--- Page Table Walk for {:#018x} ---", vaddr);

    let pml4_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    if pml4_phys >= MAX_PHYS { let _ = writeln!(w, "  (PML4 phys out of range)"); return; }
    let pml4_virt = KERNEL_VMA_BASE + pml4_phys;

    let idx4 = ((vaddr >> 39) & 0x1FF) as usize;
    let e4 = match unsafe { read_volatile_u64((pml4_virt + (idx4 as u64) * 8) as *const u64) } {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PML4 unreadable)"); return; }
    };
    write_pte_entry(w, "PML4", idx4, e4);
    if e4 & PTE_PRESENT == 0 { return; }

    let pdp_phys = e4 & 0x000F_FFFF_FFFF_F000;
    if pdp_phys >= MAX_PHYS { let _ = writeln!(w, "  (PDP phys out of range)"); return; }
    let pdp_virt = KERNEL_VMA_BASE + pdp_phys;
    let idx3 = ((vaddr >> 30) & 0x1FF) as usize;
    let e3 = match unsafe { read_volatile_u64((pdp_virt + (idx3 as u64) * 8) as *const u64) } {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PDP unreadable)"); return; }
    };
    write_pte_entry(w, "PDP", idx3, e3);
    if e3 & PTE_PRESENT == 0 { return; }
    if e3 & (1 << 7) != 0 {
        let phys = (e3 & 0x000F_FFC0_0000_0000) | (vaddr & 0x3FFF_FFFF);
        let _ = writeln!(w, "  -> 1 GiB huge page  phys={:#x}", phys);
        return;
    }

    let pd_phys = e3 & 0x000F_FFFF_FFFF_F000;
    if pd_phys >= MAX_PHYS { let _ = writeln!(w, "  (PD phys out of range)"); return; }
    let pd_virt = KERNEL_VMA_BASE + pd_phys;
    let idx2 = ((vaddr >> 21) & 0x1FF) as usize;
    let e2 = match unsafe { read_volatile_u64((pd_virt + (idx2 as u64) * 8) as *const u64) } {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PD unreadable)"); return; }
    };
    write_pte_entry(w, " PD", idx2, e2);
    if e2 & PTE_PRESENT == 0 { return; }
    if e2 & (1 << 7) != 0 {
        let phys = (e2 & 0x000F_FFFF_FE00_0000) | (vaddr & 0x1F_FFFF);
        let _ = writeln!(w, "  -> 2 MiB huge page  phys={:#x}", phys);
        return;
    }

    let pt_phys = e2 & 0x000F_FFFF_FFFF_F000;
    if pt_phys >= MAX_PHYS { let _ = writeln!(w, "  (PT phys out of range)"); return; }
    let pt_virt = KERNEL_VMA_BASE + pt_phys;
    let idx1 = ((vaddr >> 12) & 0x1FF) as usize;
    let e1 = match unsafe { read_volatile_u64((pt_virt + (idx1 as u64) * 8) as *const u64) } {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PT unreadable)"); return; }
    };
    write_pte_entry(w, " PT", idx1, e1);
    if e1 & PTE_PRESENT == 0 {
        let _ = writeln!(w, "  -> unmapped");
        return;
    }

    let phys = (e1 & 0x000F_FFFF_FFFF_F000) | (vaddr & 0xFFF);
    let _ = writeln!(w, "  -> phys={:#x}", phys);
}

// ── Main fault-dump orchestrator ───────────────────────────────────

pub fn dump_full_fault(frame: &InterruptStackFrame, error_code: u64, vector: u8) -> ! {
    if DUMP_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        loop {
            unsafe { asm!("cli", "hlt", options(nomem, nostack)); }
        }
    }

    let cr0: u64;
    let cr2: u64;
    let cr3: u64;
    let cr4: u64;
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    }

    let mut w = SerialPort;

    // ── Header ─────────────────────────────────────────────────────
    let _ = writeln!(w);
    match vector {
        14 => { let _ = writeln!(w, "==== PAGE FAULT (#14) {:=>39}", ""); }
        13 => { let _ = writeln!(w, "==== GENERAL PROTECTION (#13) {:=>29}", ""); }
        6  => { let _ = writeln!(w, "==== INVALID OPCODE (#6) {:=>35}", ""); }
        0  => { let _ = writeln!(w, "==== DIVIDE ERROR (#0) {:=>36}", ""); }
        8  => { let _ = writeln!(w, "==== DOUBLE FAULT (#8) {:=>36}", ""); }
        _  => { let _ = writeln!(w, "==== {} (#{}) {:=>50}", exception_name(vector), vector, ""); }
    }

    // ── Error code and fault address ──────────────────────────────
    let _fault_addr = if vector == 14 { cr2 } else { frame.instruction_pointer.as_u64() };

    if vector == 14 || (error_code != 0 && matches!(vector, 10..=14)) {
        dump_error_code(&mut w, vector, error_code);
        let _ = writeln!(w);
        if vector == 14 {
            let _ = writeln!(w, "CR2 (fault address): {:#018x}", cr2);
            let _ = writeln!(w);
        }
    }

    // ── CPU info ─────────────────────────────────────────────────
    write_cpuid_info(&mut w);
    let _ = writeln!(w);

    // ── Interrupt frame ───────────────────────────────────────────
    let _ = writeln!(w, "--- Interrupt Frame ---");
    let _ = writeln!(w, "RIP  = {:#018x}", frame.instruction_pointer.as_u64());
    let _ = writeln!(w, "CS   = {:#018x}", frame.code_segment.0 as u64);
    let _ = writeln!(w, "RFLAGS = {:#018x}", frame.cpu_flags.bits());
    write_rflags(&mut w, frame.cpu_flags.bits());
    let cpl = frame.code_segment.0 as u64 & 3;
    if cpl == 3 {
        let _ = writeln!(w, "SS   = {:#018x}  (saved by CPU on CPL change)", frame.stack_segment.0 as u64);
        let _ = writeln!(w, "RSP  = {:#018x}  (original user RSP)", frame.stack_pointer.as_u64());
    } else {
        let _ = writeln!(w, "RSP  = {:#018x}", frame.stack_pointer.as_u64());
    }
    let _ = writeln!(w);

    // ── Control registers ─────────────────────────────────────────
    let _ = writeln!(w, "--- Control Registers ---");
    let _ = writeln!(w, "CR0 = {:#018x}", cr0);
    write_cr0_flags(&mut w, cr0);
    let _ = writeln!(w, "CR2 = {:#018x}", cr2);
    let cr3_asid = cr3 & 0xFFF;
    let cr3_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "CR3 = {:#018x}", cr3);
    if cr3_asid != 0 {
        let _ = writeln!(w, "      phys={:#x}  ASID/PCID={:#x}", cr3_phys, cr3_asid);
    } else {
        let _ = writeln!(w, "      phys={:#x}", cr3_phys);
    }
    let _ = writeln!(w, "CR4 = {:#018x}", cr4);
    write_cr4_flags(&mut w, cr4);
    let _ = writeln!(w);

    // ── MSRs ──────────────────────────────────────────────────────
    dump_msrs(&mut w);
    let _ = writeln!(w);

    // ── RFLAGS summary ────────────────────────────────────────────
    let if_flag = (frame.cpu_flags.bits() >> 9) & 1;
    let _ = writeln!(w, "Interrupts: {}", if if_flag != 0 { "enabled (IF=1)" } else { "disabled (IF=0)" });
    let _ = writeln!(w);

    // ── Page-table walk (page faults only) ────────────────────────
    if vector == 14 {
        dump_page_walk(&mut w, cr3, cr2);
        let _ = writeln!(w);
    }

    // ── Stack dump ────────────────────────────────────────────────
    let rsp = frame.stack_pointer.as_u64();
    dump_fault_stack(&mut w, rsp, cr3);
    let _ = writeln!(w);

    // ── Code disassembly ──────────────────────────────────────────
    let rip = frame.instruction_pointer.as_u64();
    dump_code_bytes(&mut w, rip, cr3);
    let _ = writeln!(w);

    // ── Footer ────────────────────────────────────────────────────
    let _ = writeln!(w, "================================================");

    DUMP_IN_PROGRESS.store(false, Ordering::Release);

    loop {
        unsafe { asm!("cli", "hlt", options(nomem, nostack)); }
    }
}
