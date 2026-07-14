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
    rsdp_addr: u64,
) -> ! {
    // Reinit COM1 — boot already did this, but be safe
    SerialPort::init();
    SerialPort::puts("[kernel] _start entered\n");

    #[cfg(feature = "cpu_slow")]
    {
        SerialPort::puts("[kernel] Enabling CPU slow mode...\n");
        unsafe { kernel::arch::x86_64::limiter::enable_cpu_slow_mode() };
    }

    // Validate pointers from bootloader before dereferencing
    assert!(!memory_map_ptr.is_null(), "memory_map_ptr is null");
    assert!(!framebuffer_ptr.is_null(), "framebuffer_ptr is null");
    SerialPort::puts("[kernel] Pointers OK\n");

    let framebuffer = unsafe { &*framebuffer_ptr };
    let memory_map = unsafe {
        core::slice::from_raw_parts(memory_map_ptr, memory_map_len)
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel = unsafe { kernel::Kernel::new(memory_map, framebuffer, stack_guard, rsdp_addr) };
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
    /* Atomic boot lock: only the first hart to claim this proceeds */
    la t0, _boot_lock
    li t1, 1
    amoswap.w t2, t1, 0(t0)
    bnez t2, park

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

park:
    /* Stop this hart via SBI HSM so the BSP can wake us with hart_start */
    li a7, 0x48534D
    li a6, 2
    ecall
    wfi
    j park

    .section .data
    .balign 4
_boot_lock:
    .word 0
"#,
);

#[cfg(target_arch = "riscv64")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_entry(hart_id: u64, dtb_ptr: *const u8) -> ! {
    use kernel::boot::PixelFormat;
    SerialPort::init();
    SerialPort::puts("[kernel] riscv64 _start entered, hart_id=");
    SerialPort::put_u64(hart_id);
    SerialPort::puts("\n");

    // Store DTB pointer for later use (SMP discovery, etc.).
    kernel::platform::riscv_virt::set_dtb_ptr(dtb_ptr);
    // Store hart_id for PLIC (reads mhartid are illegal in S-mode).
    use core::sync::atomic::Ordering;
    kernel::platform::riscv_virt::plic::HART_ID.store(hart_id as usize, Ordering::Relaxed);

    // Debug: print DTB pointer and first 8 bytes.
    SerialPort::puts("[kernel] DTB ptr=0x");
    SerialPort::put_hex(dtb_ptr as u64);
    SerialPort::puts(", magic=0x");
    if !dtb_ptr.is_null() {
        SerialPort::put_hex(unsafe {
            u64::from(
                (core::ptr::read_volatile(dtb_ptr) as u32) << 24
                    | (core::ptr::read_volatile(dtb_ptr.add(1)) as u32) << 16
                    | (core::ptr::read_volatile(dtb_ptr.add(2)) as u32) << 8
                    | (core::ptr::read_volatile(dtb_ptr.add(3)) as u32)
            )
        });
    } else {
        SerialPort::puts("NULL");
    }
    SerialPort::puts("\n");

    // Parse memory map and RSDP from DTB via the dedicated module.
    let memory_map = kernel::dtb::parse_memory(dtb_ptr);
    let rsdp_addr = kernel::dtb::find_rsdp(dtb_ptr);

    // Compute stack guard: one unmapped page just below the stack area.
    let stack_guard = unsafe {
        let stack_start = &kernel::__stack_start as *const u8 as u64;
        stack_start - 4096
    };

    static FB_INFO: FramebufferInfo = FramebufferInfo {
        address: 0, width: 0, height: 0, stride: 0,
        pixel_format: PixelFormat::Bgr,
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel = unsafe { kernel::Kernel::new(memory_map, &FB_INFO, stack_guard, rsdp_addr) };
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
    // Disable interrupts and spin forever with wfi to prevent reboot.
    CurrentArch::disable_interrupts();
    loop {
        // wfi may be a NOP when interrupts are disabled on some implementations;
        // busy-wait with a fence to prevent the compiler from optimising the
        // loop away entirely.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        CurrentArch::halt();
    }
}
