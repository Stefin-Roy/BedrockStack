use core::arch::global_asm;

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
_start:
    and  rsp, -16
    call __libc_init        # wire up std streams, etc.
    call entry_main
    mov  rdi, rax           # entry_main status -> exit code
    call exit_process       # write /proc/self:exit, never returns
    hlt
"#
);

/// Called by `_start` before the app's `entry_main`. Sets up the standard
/// stream handles and any other one-time libc state.
#[unsafe(no_mangle)]
pub extern "C" fn __libc_init() {
    crate::stdio::stdio_init();
    crate::vfs::vfs_init();
}

#[unsafe(no_mangle)]
pub extern "C" fn exit_process(code: usize) -> ! {
    crate::process::exit(code)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
