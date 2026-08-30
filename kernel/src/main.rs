#![no_std]
#![no_main]

#[cfg(target_arch = "riscv64")]
use core::arch::global_asm;
use core::panic::PanicInfo;
#[allow(unused_imports)]
use kernel::arch::CurrentArch;
#[cfg(any(target_arch = "riscv64", not(feature = "kernelmb2")))]
use kernel::boot::FramebufferInfo;
#[cfg(all(target_arch = "x86_64", not(feature = "kernelmb2")))]
use kernel::boot::MemoryRegion;
use kernel::drivers::serial::SerialPort;

/// Kernel entry point (custom bootloader path).
///
/// Runs at KERNEL_VMA (higher half): the low assembly `_start` stub in
/// `.text.boot64` installs CR3 = `__boot_pml4` and long-jumps here.
///
/// # Safety
/// Called from boot after exit_boot_services.
#[unsafe(no_mangle)]
#[cfg(not(feature = "kernelmb2"))]
#[cfg(target_arch = "x86_64")]
pub extern "sysv64" fn kernel_main(
    memory_map_ptr: *const MemoryRegion,
    memory_map_len: usize,
    framebuffer_ptr: *const FramebufferInfo,
    _stack_guard: u64,
    rsdp_addr: u64,
) -> ! {
    // ── Kernel arrived ──

    // Reinit COM1 — boot already did this, but be safe
    SerialPort::init();
    SerialPort::puts("[kernel] _start entered\n");

    // The low stub switched RSP to the kernel's high `.stack` after loading
    // CR3, so the guard page is the frame below it — NOT the bootloader's
    // stack guard (that stack is never used; the stub overrides RSP).
    let stack_guard = unsafe { &kernel::__stack_start_phys as *const u8 as u64 - 4096 };

    #[cfg(feature = "cpu_slow")]
    {
        SerialPort::puts("[kernel] Enabling CPU slow mode...\n");
        unsafe { kernel::arch::x86_64::limiter::enable_cpu_slow_mode() };
    }

    // UEFI has no Multiboot2 cmdline – mark bootargs as empty so
    // `is_nokaslr()` is well-defined (always false here).
    kernel::bootargs::init_empty();

    // Validate pointers from bootloader before dereferencing
    assert!(!memory_map_ptr.is_null(), "memory_map_ptr is null");
    assert!(!framebuffer_ptr.is_null(), "framebuffer_ptr is null");
    SerialPort::puts("[kernel] Pointers OK\n");

    let framebuffer = unsafe { &*framebuffer_ptr };
    let memory_map = unsafe { core::slice::from_raw_parts(memory_map_ptr, memory_map_len) };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel =
        unsafe { kernel::Kernel::new(memory_map, framebuffer, stack_guard, rsdp_addr, None) };
    SerialPort::puts("[kernel] Init...\n");
    kernel.init();
    kernel.run();
}

// Low 64-bit `_start` stub for the UEFI bootloader path (non-`kernelmb2`).
//
// The kernel is linked higher-half, so `e_entry` must be a LOW stub that
// installs the static `.boottables` (CR3 = `__boot_pml4`) and long-jumps
// into the high `kernel_main`.  The UEFI-provided sysv64 args
// (rdi/rsi/rdx/rcx/r8 = memory_map_ptr/len/framebuffer/stack_guard/rsdp)
// are already in exactly the registers `kernel_main` expects, and none of
// the setup instructions below clobber them, so they pass straight through.
#[cfg(all(target_arch = "x86_64", not(feature = "kernelmb2")))]
core::arch::global_asm!(
    r#"
    .section .text.boot64, "ax"
    .code64
.globl _start
_start:
    // Low `.bootstack` — the kernel `.stack` is higher-half and unmapped
    // before the CR3 switch, so the stub stacks onto the low boot stack.
    lea rsp, [rip + __boot_stack_end]
    xor rbp, rbp

    // Install the static higher-half page tables.  `__boot_pml4` is a low
    // region symbol whose value == its physical address (identity mapped),
    // and this stub is still running low, so the switch is safe.
    movabs rax, offset __boot_pml4
    mov cr3, rax

    // Switch to the kernel's high `.stack` — `.boottables` maps the whole
    // [KERNEL_VMA, +256 MiB) window RW, and `.stack` (ending at `__kernel_end`)
    // lies inside it, so it is reachable the moment CR3 is loaded.  The low
    // `.bootstack` is dead from here on; the kernel runs its entire life on
    // this high stack, which every domain's cloned high half maps.
    movabs rax, offset __stack_end
    mov rsp, rax

    // Far-into-high jump: `kernel_main` is a kernel-region symbol (high VMA)
    // that `.boottables` maps at [KERNEL_VMA, +256 MiB).  The 5 sysv64 args
    // in rdi/rsi/rdx/rcx/r8 are untouched and match kernel_main's signature.
    movabs rax, offset kernel_main
    jmp rax
"#,
);

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
                    | (core::ptr::read_volatile(dtb_ptr.add(3)) as u32),
            )
        });
    } else {
        SerialPort::puts("NULL");
    }
    SerialPort::puts("\n");

    // RISC-V has no Multiboot2 – mark bootargs empty.
    kernel::bootargs::init_empty();

    // Parse memory map and RSDP from DTB via the dedicated module.
    let memory_map = kernel::dtb::parse_memory(dtb_ptr);
    let rsdp_addr = kernel::dtb::find_rsdp(dtb_ptr);

    // Compute stack guard: one unmapped page just below the stack area.
    let stack_guard = unsafe {
        let stack_start = &kernel::__stack_start as *const u8 as u64;
        stack_start - 4096
    };

    static FB_INFO: FramebufferInfo = FramebufferInfo {
        address: 0,
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: PixelFormat::Bgr,
        bpp: 4,
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel =
        unsafe { kernel::Kernel::new(memory_map, &FB_INFO, stack_guard, rsdp_addr, None) };
    SerialPort::puts("[kernel] Init...\n");
    kernel.init();
    kernel.run();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // SMP-aware panic: freeze peers via NMI, dump all CPU stacks/MSRs, then halt.
    // Use lock-free dump path so we never deadlock on SerialPort spinlocks when
    // panic fires with IF=0 or inside an IrqMutex.
    #[cfg(target_arch = "x86_64")]
    {
        // Check for panic-while-panic (nested panic during dump) — just halt to avoid triple fault.
        let _cpu = kernel::smp::current_cpu_id() as usize;
        if kernel::kerneldump::dump::is_dump_in_progress() {
            kernel::drivers::serial::SerialPort::puts("\n*** NESTED PANIC while dumping — halting\n");
            loop { unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) }; }
        }
        // Build panic message into a small stack buffer for dump_fatal context.
        // dump_fatal will synthesize a frame and invoke the full SMP dump (freeze + all CPUs).
        // We still emit the panic location first via lock-free path. Avoid heap alloc here —
        // panic may have corrupted the heap, so use only stack + dump_puts.
        {
            use kernel::drivers::serial::{dump_puts, dump_put_hex};
            dump_puts("\n*** KERNEL PANIC: ");
            if let Some(loc) = info.location() {
                dump_puts(loc.file());
                dump_puts(":");
                dump_put_hex(loc.line() as u64);
                dump_puts(" ");
            }
            dump_puts(" (see SMP dump below)\n");
            // Also try to emit panic message via lock-free writer (best effort, no alloc)
            // Use a tiny stack buffer and fmt::write to dump_puts.
            struct DumpSink;
            impl core::fmt::Write for DumpSink {
                fn write_str(&mut self, s: &str) -> core::fmt::Result { dump_puts(s); Ok(()) }
            }
            let _ = core::fmt::write(&mut DumpSink, format_args!("{}", info.message()));
            dump_puts("\n");
        }
        // Try SMP freeze dump — this never returns (halts). It will print stage, last log, registers, stacks, MSRs for all CPUs.
        // Use static context to avoid alloc; dump_fatal requires 'static str.
        kernel::kerneldump::dump::dump_fatal("panic");
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
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
        SerialPort::puts("--- Kernel Stage ---\n");
        SerialPort::puts("Stage: ");
        SerialPort::puts(kernel::stage::as_str());
        SerialPort::puts(" (");
        SerialPort::puts(kernel::stage::bootanim_str());
        SerialPort::puts(")\n");
        SerialPort::puts("--- Last Log (4 lines) ---\n");
        {
            struct PanicWriter;
            impl Write for PanicWriter {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    SerialPort::puts(s);
                    Ok(())
                }
            }
            let mut w = PanicWriter;
            if !kernel::drivers::serial::try_dump_last_lines(&mut w, 4) {
                let _ = write!(w, "(log unavailable: contended)\n");
            }
        }
        CurrentArch::disable_interrupts();
        loop {
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            CurrentArch::halt();
        }
    }
}
