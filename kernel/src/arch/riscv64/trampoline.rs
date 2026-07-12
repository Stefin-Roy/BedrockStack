//! RISC-V SMP trampoline — supervisor-mode startup for Application Processors.
//!
//! SBI HSM (`hart_start`) wakes each AP at the trampoline physical address
//! with `a0 = hart_id`, `a1 = PerCpu pointer`.  The trampoline sets `tp` and
//! `sp`, then jumps to the Rust `ap_entry_riscv` which enables the MMU.

use crate::arch::Arch;
use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;
use crate::smp::{ApContext, PerCpu, per_cpu_by_id, current_per_cpu};

pub const TRAMPOLINE_ADDR: u64 = 0x8000;
const DATA_OFFSET: u64 = 0x700; // trampoline data at 0x8700

/// Data block at 0x8700 (written by BSP, read by AP Rust entry).
#[repr(C)]
struct TrampolineData {
    satp: u64,
    entry: u64,
}

// ── Trampoline assembly ──────────────────────────────────────────────
// Just sets tp, sp and jumps to the Rust entry point stored in the data
// block.  All MMU setup happens in `ap_entry_riscv` (kernel text).
core::arch::global_asm!(
    ".pushsection .text.trampoline, \"ax\"",
    ".balign 16",
    ".globl _trampoline_start",
    "_trampoline_start:",
    // a0 = hart_id (ignored), a1 = per_cpu_ptr (from priv)
    "mv   tp, a1",
    // Load stack_top from PerCpu (offset 32 = u64 after started)
    "ld   sp, 32(tp)",
    // Jump to Rust entry point stored in TrampolineData at 0x8700+8
    "li   t0, 0x8700",
    "ld   t1, 8(t0)",
    "jr   t1",
    ".globl _trampoline_end",
    "_trampoline_end:",
    ".popsection",
);

pub unsafe fn start_aps(
    _allocator: &mut BitmapAllocator,
    page_table_root: u64,
    aps: &[ApContext],
) -> usize {
    SerialPort::puts("[trampoline] start_aps\n");

    unsafe extern "C" {
        static _trampoline_start: u8;
        static _trampoline_end: u8;
    }
    let src = unsafe { &_trampoline_start as *const u8 as u64 };
    let end = unsafe { &_trampoline_end as *const u8 as u64 };
    let size = (end - src) as usize;

    assert!(size <= 0x1000, "trampoline too large");

    _allocator.reserve_region(TRAMPOLINE_ADDR, TRAMPOLINE_ADDR + 0x1000);
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, TRAMPOLINE_ADDR as *mut u8, size);
    }

    let entry = ap_entry_riscv as usize as u64;
    let data = (TRAMPOLINE_ADDR + DATA_OFFSET) as *mut TrampolineData;

    // Build the satp value from the page table root.
    let satp = (8u64 << 60) | (page_table_root >> 12);

    let mut started_ok = 0usize;

    for ap in aps {
        SerialPort::puts("[trampoline] waking AP cpu_id=");
        SerialPort::put_u64(ap.cpu_id as u64);
        SerialPort::puts(" hart_id=");
        SerialPort::put_u64(ap.hardware_id as u64);
        SerialPort::puts("\n");

        let pc: &mut PerCpu = per_cpu_by_id(ap.cpu_id);
        let pc_ptr = pc as *const PerCpu as u64;

        unsafe {
            data.write(TrampolineData { satp, entry });
        }

        // Ensure all TrampolineData and PerCpu writes are visible before AP wakes.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        if !crate::arch::riscv64::sbi::hart_start(ap.hardware_id as u64, TRAMPOLINE_ADDR, pc_ptr) {
            SerialPort::puts("[trampoline] WARNING: hart_start failed\n");
            continue;
        }

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

/// Rust entry point for APs — called by the trampoline with tp already set.
///
/// The MMU is NOT yet enabled; we enable it here by reading the satp value
/// from the TrampolineData block at physical 0x8700.
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry_riscv() -> ! {
    // Read satp from the data block (physical address, accessible in Bare mode).
    let data = (TRAMPOLINE_ADDR + DATA_OFFSET) as *const u64;
    let satp = unsafe { core::ptr::read_volatile(data) };

    // Enable MMU.
    unsafe {
        core::arch::asm!("csrw satp, {}", in(reg) satp);
        core::arch::asm!("sfence.vma");
    }

    let pc = current_per_cpu();
    let cpu_id = pc.cpu_id;

    SerialPort::puts("[AP] cpu ");
    SerialPort::put_u64(cpu_id as u64);
    SerialPort::puts(" online\n");

    // Mark started.
    pc.started.store(1, core::sync::atomic::Ordering::Release);

    // Per-CPU arch init (trap vectors, PLIC, SIE).
    crate::arch::CurrentArch::init_ap(cpu_id);
    crate::arch::CurrentArch::enable_interrupts();

    loop {
        crate::arch::CurrentArch::halt();
    }
}
