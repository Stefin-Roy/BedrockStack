#![no_std]
#![no_main]

#[cfg(target_arch = "riscv64")]
use core::arch::global_asm;
use core::panic::PanicInfo;
use kernel::arch::{Arch, CurrentArch};
use kernel::boot::{FramebufferInfo, MemoryRegion};
use kernel::drivers::serial::SerialPort;

/// Kernel entry point.
///
/// # Safety
/// Called from boot after exit_boot_services.
#[unsafe(no_mangle)]
#[cfg(target_arch = "x86_64")]
pub extern "sysv64" fn _start(
    memory_map_ptr: *const MemoryRegion,
    memory_map_len: usize,
    framebuffer_ptr: *const FramebufferInfo,
    stack_guard: u64,
) -> ! {
    // Reinit COM1 — boot already did this, but be safe
    SerialPort::init();
    SerialPort::puts("[kernel] _start entered\n");

    // Validate pointers from bootloader before dereferencing
    assert!(!memory_map_ptr.is_null(), "memory_map_ptr is null");
    assert!(!framebuffer_ptr.is_null(), "framebuffer_ptr is null");
    SerialPort::puts("[kernel] Pointers OK\n");

    let framebuffer = unsafe { &*framebuffer_ptr };
    let memory_map = unsafe {
        core::slice::from_raw_parts(memory_map_ptr, memory_map_len)
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel = unsafe { kernel::Kernel::new(memory_map, framebuffer, stack_guard) };
    SerialPort::puts("[kernel] Init...\n");
    kernel.init();
    SerialPort::puts("[kernel] Init complete, running modules...\n");
    kernel.run();
}

#[cfg(target_arch = "riscv64")]
global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    /* Write '>' directly to UART at 0x10000000 */
    li t0, 0x10000000
1:  lbu t1, 5(t0)
    andi t1, t1, 0x20
    beqz t1, 1b
    li t1, 62
    sb t1, 0(t0)

    /* Set stack pointer */
    la sp, __stack_end
    mv s0, zero

    /* Zero BSS */
    la t0, __bss_start
    la t1, __bss_end
2:  beq t0, t1, 3f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 2b
3:
    /* Jump to the Rust entry point (a0=hart_id, a1=dtb). */
    tail rust_entry
"#,
);

#[cfg(target_arch = "riscv64")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_entry(hart_id: u64, _dtb_ptr: *const u8) -> ! {
    use kernel::boot::{MemoryRegionKind, PixelFormat};
    SerialPort::init();
    SerialPort::puts("[kernel] riscv64 _start entered, hart_id=");
    SerialPort::put_u64(hart_id);
    SerialPort::puts("\n");

    // RAM: 256MB at 0x80000000. OpenSBI firmware lives in 0x80000000–0x8004FFFF
    // (PMP-protected, S-mode cannot access).  Start usable memory after it.
    static MEMORY_REGIONS: [MemoryRegion; 3] = [
        MemoryRegion { base: 0x80050000, size: 0x0FFB0000, kind: MemoryRegionKind::Usable },
        MemoryRegion { base: 0x00100000, size: 0x00001000, kind: MemoryRegionKind::Reserved },
        MemoryRegion { base: 0x80000000, size: 0x00050000, kind: MemoryRegionKind::Reserved },
    ];
    static FB_INFO: FramebufferInfo = FramebufferInfo {
        address: 0, width: 0, height: 0, stride: 0,
        pixel_format: PixelFormat::Bgr,
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel = unsafe { kernel::Kernel::new(&MEMORY_REGIONS, &FB_INFO, 0) };
    SerialPort::puts("[kernel] Init...\n");
    kernel.init();
    SerialPort::puts("[kernel] Init complete, running modules...\n");
    kernel.run();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    SerialPort::puts("\n*** KERNEL PANIC: ");
    if let Some(loc) = info.location() {
        SerialPort::puts(loc.file());
        SerialPort::puts(":");
        SerialPort::put_u64(loc.line() as u64);
        SerialPort::puts(" ");
    }
    use core::fmt::Write;
    let _ = write!(SerialPort::new(), "{}", info.message());
    SerialPort::puts("\n");
    loop {
        CurrentArch::disable_interrupts();
        CurrentArch::halt();
    }
}
