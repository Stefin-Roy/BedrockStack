//! BedrockOS userspace init.
//!
//! Runs in ring 3 via `sysretq`. Exercises the syscall ABI (version 1 table:
//! syscall number in RAX, args in RDI/RSI/RDX, table version in R10) and then
//! exits. GS is kernel-owned — user code must never touch it.

#![no_std]
#![no_main]

use core::arch::asm;

/// Syscall numbers (version 1 table).
const SYS_WRITE: u64 = 0;
const SYS_EXIT: u64 = 1;

/// Table version passed in R10 on every syscall.
const TABLE_VERSION: u64 = 1;

/// Entry point jumped to by the kernel (sysretq from ring 0).
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Say hello through the syscall path (fd ignored, goes to serial).
    let msg = b"Hello from ring 3!\n";
    unsafe {
        syscall(SYS_WRITE, 1, msg.as_ptr() as u64, msg.len() as u64);
    }

    // Second write to prove the round trip keeps working.
    let msg2 = b"init: syscall round trip OK\n";
    unsafe {
        syscall(SYS_WRITE, 1, msg2.as_ptr() as u64, msg2.len() as u64);
    }

    // Exit back into the kernel idle loop.
    unsafe {
        syscall(SYS_EXIT, 0, 0, 0);
    }

    unreachable!("sys_exit never returns");
}

/// Raw `syscall` invocation.
///
/// # Safety
/// `num`/`args` must match a valid entry in the version-1 syscall table.
unsafe fn syscall(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") TABLE_VERSION,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Panic handler — ring 3 has no unwind; just halt.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe {
            asm!("hlt");
        }
    }
}
