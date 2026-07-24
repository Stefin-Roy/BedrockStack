//! x86_64 SMP trampoline — real-mode startup code for Application Processors.

use crate::arch::Arch;
use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::platform::x86_64_pc::apic;
use crate::smp::{ApContext, current_per_cpu};

pub const TRAMPOLINE_ADDR: u64 = 0x8000;
const DATA_OFFSET: u64 = 0x700;
pub const TRAMPOLINE_PAGE: u8 = (TRAMPOLINE_ADDR >> 12) as u8;

/// Data block at 0x8700 — written by BSP, read by AP trampoline.
#[repr(C)]
struct TrampolineData {
    cr3: u64,
    stack_top: u64,
    entry: u64,
    per_cpu_ptr: u64,
    started_flag_addr: u64,
    lm_entry: u64,
}

core::arch::global_asm!(
    ".pushsection .text.trampoline, \"ax\"",
    ".globl _trampoline_start",
    "_trampoline_start:",

    ".code16",
    "cli",
    "cld",
    "xor  ax, ax",
    "mov  ds, ax",
    "mov  es, ax",
    "mov  ss, ax",
    "mov  sp, 0x7000",

    "in   al, 0x92",
    "or   al, 2",
    "out  0x92, al",

    ".byte 0x0f, 0x01, 0x16",
    ".word _trampoline_gdt_ptr - _trampoline_start + 0x8000",

    "mov  eax, cr0",
    "or   al, 1",
    "mov  cr0, eax",

    ".byte 0xea",
    ".word (_trampoline_pm - _trampoline_start + 0x8000) & 0xFFFF",
    ".word 0x08",

    ".code32",
    "_trampoline_pm:",
    "mov  ax, 0x10",
    "mov  ds, ax",
    "mov  es, ax",
    "mov  ss, ax",

    // Load CR3 *before* enabling paging (32-bit mode).  On INIT+SIPI the AP's
    // CR3 is 0 (from reset); enabling paging without a valid PML4 would fault.
    // We load again in 64-bit mode below in case the value was truncated.
    "mov  eax, [0x8700]",
    "mov  cr3, eax",

    "mov  eax, cr4",
    "or   eax, 1 << 5",
    "mov  cr4, eax",

    "mov  ecx, 0xC0000080",
    "rdmsr",
    "or   eax, 1 << 8",                   // LME
    "wrmsr",

    "mov  eax, cr0",
    "or   eax, 0x80000000",
    "mov  cr0, eax",

    // Now LMA = 1 (set automatically when paging enabled with LME=1), so we
    // can safely set NXE (IA32_EFER[11]) without a #GP.
    "mov  ecx, 0xC0000080",
    "rdmsr",
    "or   eax, 1 << 11",                  // NXE
    "wrmsr",

    // The trampoline data (0x8700–0x872F) is identity-mapped in the kernel
    // page table, so it is still accessible here in compatibility mode.
    "mov  eax, [0x8728]",
    "push 0x18",
    "push eax",
    "retf",

    ".code64",
    "_trampoline_lm:",

    // Re-load CR3 with the full 64-bit value in case step above truncated it.
    "mov  rax, [0x8700]",
    "mov  cr3, rax",

    // Atomically claim a boot index from the shared counter at 0x8E00.
    "lock inc qword ptr [0x8E00]",
    "mov  rbx, [0x8E00]",
    "dec  rbx",

    // Index into per-AP records at 0x8D00 + index * 64.
    "shl  rbx, 6",
    "add  rbx, 0x8D00",

    // Load stack_top from record offset 8 and set RSP.
    "mov  rsp, [rbx + 8]",
    "and  rsp, -16",
    "sub  rsp, 8",

    // Set GS.base from per_cpu_ptr at record offset 0.
    "mov  ecx, 0xC0000101",
    "mov  rax, [rbx]",
    "mov  rdx, rax",
    "shr  rdx, 32",
    "wrmsr",

    // Jump into Rust — ap_entry64 uses current_per_cpu() directly.
    "mov  rax, [0x8710]",
    "jmp  rax",

    ".balign 8",
    "_trampoline_gdt:",
    ".quad 0x0000000000000000",
    ".quad 0x00CF9A000000FFFF",
    ".quad 0x00CF92000000FFFF",
    ".quad 0x00AF9A000000FFFF",
    "_trampoline_gdt_end:",

    ".balign 4",
    "_trampoline_gdt_ptr:",
    ".word _trampoline_gdt_end - _trampoline_gdt - 1",
    ".long _trampoline_gdt - _trampoline_start + 0x8000",

    ".globl _trampoline_end",
    "_trampoline_end:",
    ".popsection",
);

pub unsafe fn start_aps(
    allocator: &mut BitmapAllocator,
    page_table_root: u64,
    aps: &[ApContext],
) -> usize {
    SerialPort::puts("[trampoline] start_aps\n");

    unsafe extern "C" {
        static _trampoline_start: u8;
        static _trampoline_lm: u8;
        static _trampoline_end: u8;
    }
    let src = unsafe { &_trampoline_start as *const u8 as u64 };
    let lm_phys = unsafe { TRAMPOLINE_ADDR + (&_trampoline_lm as *const u8 as u64 - src) };
    let end = unsafe { &_trampoline_end as *const u8 as u64 };
    let size = (end - src) as usize;

    assert!(size <= 0x1000, "trampoline too large");

    allocator.reserve_region(TRAMPOLINE_ADDR, TRAMPOLINE_ADDR + 0x1000);
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, TRAMPOLINE_ADDR as *mut u8, size);
    }

    let entry = ap_entry64 as *const () as usize as u64;
    let data = (TRAMPOLINE_ADDR + DATA_OFFSET) as *mut TrampolineData;

    // Write per-AP boot records at 0x8D00 (within the reserved trampoline page).
    // Each record is 64 bytes (cache-line sized) to avoid false sharing when
    // multiple APs read concurrently.
    for (i, ap) in aps.iter().enumerate() {
        let pc = crate::smp::per_cpu_by_id(ap.cpu_id);
        let record_addr = (0x8D00 + (i as u64) * 64) as *mut u64;
        unsafe {
            *record_addr = pc as *const crate::smp::PerCpu as u64;
            *record_addr.add(1) = ap.stack_top;
        }
    }
    // Reset atomic boot counter at 0x8E00.
    unsafe { *(0x8E00 as *mut u64) = 0; }

    // Write shared trampoline data once (cr3, entry, lm_entry are the same for all APs).
    unsafe {
        data.write(TrampolineData {
            cr3: page_table_root,
            stack_top: 0,          // unused — AP self-configures
            entry,
            per_cpu_ptr: 0,        // unused — AP self-identifies
            started_flag_addr: 0,  // unused
            lm_entry: lm_phys,
        });
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    // Phase 1: INIT all APs in parallel
    SerialPort::puts("[trampoline] Phase 1: INIT all APs\n");
    for ap in aps {
        SerialPort::puts("  >>> send_init_ipi cpu_id=");
        SerialPort::put_u64(ap.cpu_id as u64);
        SerialPort::puts(" hardware_id=");
        SerialPort::put_u64(ap.hardware_id as u64);
        SerialPort::puts("\n");
        crate::platform::x86_64_pc::apic::send_init_ipi(ap.hardware_id);
    }
    apic_delay_ms(10);

    // Phase 2: Deassert + SIPI each AP
    SerialPort::puts("[trampoline] Phase 2: SIPI all APs\n");
    for ap in aps {
        crate::platform::x86_64_pc::apic::send_init_deassert(ap.hardware_id);
        apic_delay_us(200);
        crate::platform::x86_64_pc::apic::send_sipi_ipi(ap.hardware_id, TRAMPOLINE_PAGE);
        apic_delay_us(200);
        crate::platform::x86_64_pc::apic::send_sipi_ipi(ap.hardware_id, TRAMPOLINE_PAGE);
    }

    // Phase 3: Parallel poll with 30ms timeout
    SerialPort::puts("[trampoline] Phase 3: polling\n");
    let mut timeout = apic::ApicTimeout::new(30);
    let mut started_ok = 0usize;
    while started_ok < aps.len() {
        if timeout.expired() {
            SerialPort::puts("[trampoline] poll timeout\n");
            break;
        }
        started_ok = 0;
        for ap in aps {
            if crate::smp::AP_READY[ap.cpu_id as usize].ready
                .load(core::sync::atomic::Ordering::Acquire)
            {
                started_ok += 1;
            }
        }
        core::hint::spin_loop();
    }

    for ap in aps {
        if crate::smp::AP_READY[ap.cpu_id as usize].ready
            .load(core::sync::atomic::Ordering::Acquire)
        {
            SerialPort::puts("[trampoline] AP ");
            SerialPort::put_u64(ap.cpu_id as u64);
            SerialPort::puts(" started OK\n");
        } else {
            SerialPort::puts("[trampoline] WARNING: AP ");
            SerialPort::put_u64(ap.cpu_id as u64);
            SerialPort::puts(" startup TIMEOUT\n");
        }
    }

    started_ok
}

#[unsafe(no_mangle)]
pub extern "C" fn ap_entry64() -> ! {
    let pc = current_per_cpu();
    let cpu_id = pc.cpu_id;

    // Signal ready — own cache line, no lock needed.
    crate::smp::AP_READY[cpu_id as usize].ready.store(true, core::sync::atomic::Ordering::Release);

    SerialPort::puts("[AP] cpu ");
    SerialPort::put_u64(cpu_id as u64);
    SerialPort::puts(" online\n");

    #[cfg(feature = "cpu_slow")]
    {
        SerialPort::puts("[AP] Enabling CPU slow mode...\n");
        unsafe { crate::arch::x86_64::limiter::enable_cpu_slow_mode() };
    }

    // Per-CPU GDT + TSS (double-fault IST stack).  This reloads CS/DS/ES/SS
    // from the real kernel GDT and loads the task register.
    crate::arch::x86_64::gdt::init();

    // Reload the BSP's IDT on this AP (IDTR is per-CPU).
    crate::arch::x86_64::idt::init_ap();

    // Initialise this CPU's local APIC.
    crate::arch::CurrentArch::init_ap(cpu_id);

    // Now safe to enable interrupts — GDT, TSS, and IDT are all set.
    crate::arch::CurrentArch::enable_interrupts();

    loop {
        crate::arch::CurrentArch::halt();
    }
}

fn apic_delay_ms(ms: u64) {
    let mut t = apic::ApicTimeout::new(ms);
    while !t.expired() {
        core::hint::spin_loop();
    }
}

/// Busy-wait for at least `us` microseconds.
///
/// The APIC timer has 1ms granularity, so sub-millisecond waits round up
/// to the next 1ms boundary. This is fine — the MP spec only requires a
/// *minimum* delay of 200µs between INIT and SIPI.
fn apic_delay_us(_us: u64) {
    apic_delay_ms(1);
}
