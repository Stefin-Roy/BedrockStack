//! x86_64 SMP trampoline — real-mode startup code for Application Processors.

use crate::arch::Arch;
use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::smp::{ApContext, PerCpu, per_cpu_by_id, current_per_cpu};

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

    "mov  eax, [0x8700]",
    "mov  cr3, eax",

    "mov  eax, cr4",
    "or   eax, 1 << 5",
    "mov  cr4, eax",

    "mov  ecx, 0xC0000080",
    "rdmsr",
    "or   eax, 1 << 8",
    "wrmsr",

    "mov  eax, cr0",
    "or   eax, 0x80000000",
    "mov  cr0, eax",

    "mov  eax, [0x8728]",
    "push 0x18",
    "push eax",
    "retf",

    ".code64",
    "_trampoline_lm:",
    "mov  rsp, [0x8708]",

    // Reload CR3 with full 64-bit value (in 32-bit mode only low 32 bits were loaded).
    "mov  rax, [0x8700]",
    "mov  cr3, rax",

    "mov  ecx, 0xC0000101",
    "mov  rax, [0x8718]",
    "mov  rdx, rax",
    "shr  rdx, 32",
    "wrmsr",

    "mov  rax, [0x8720]",
    "mfence",
    "mov  qword ptr [rax], 1",

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

    let mut started_ok = 0usize;

    for ap in aps {
        SerialPort::puts("[trampoline] waking AP cpu_id=");
        SerialPort::put_u64(ap.cpu_id as u64);
        SerialPort::puts(" hardware_id=");
        SerialPort::put_u64(ap.hardware_id as u64);
        SerialPort::puts("\n");

        let pc: &mut PerCpu = per_cpu_by_id(ap.cpu_id);
        let started_addr = &pc.started as *const core::sync::atomic::AtomicU64 as u64;

        unsafe {
            data.write(TrampolineData {
                cr3: page_table_root,
                stack_top: ap.stack_top,
                entry,
                per_cpu_ptr: pc as *const PerCpu as u64,
                started_flag_addr: started_addr,
                lm_entry: lm_phys,
            });
        }

        // Ensure all TrampolineData and PerCpu writes are visible before AP wakes.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        crate::platform::x86_64_pc::apic::send_init_ipi(ap.hardware_id as u8);
        delay_ms(10);

        crate::platform::x86_64_pc::apic::send_init_deassert(ap.hardware_id as u8);
        delay_us(200);

        crate::platform::x86_64_pc::apic::send_sipi_ipi(ap.hardware_id as u8, TRAMPOLINE_PAGE);
        delay_us(200);
        crate::platform::x86_64_pc::apic::send_sipi_ipi(ap.hardware_id as u8, TRAMPOLINE_PAGE);

        for _ in 0..200_000_000 {
            if pc.started.load(core::sync::atomic::Ordering::Acquire) != 0 {
                break;
            }
            core::hint::spin_loop();
        }

        if pc.started.load(core::sync::atomic::Ordering::Acquire) != 0 {
            started_ok += 1;
            SerialPort::puts("[trampoline] AP started OK\n");
        } else {
            SerialPort::puts("[trampoline] WARNING: AP startup TIMEOUT\n");
        }
    }

    started_ok
}

#[unsafe(no_mangle)]
pub extern "C" fn ap_entry64() -> ! {
    let pc = current_per_cpu();
    let cpu_id = pc.cpu_id;

    SerialPort::puts("[AP] cpu ");
    SerialPort::put_u64(cpu_id as u64);
    SerialPort::puts(" online\n");

    crate::arch::CurrentArch::init_ap(cpu_id);
    crate::arch::CurrentArch::enable_interrupts();

    loop {
        crate::arch::CurrentArch::halt();
    }
}

fn delay_ms(ms: u64) {
    for _ in 0..ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

fn delay_us(us: u64) {
    for _ in 0..us * 1_000 {
        core::hint::spin_loop();
    }
}
