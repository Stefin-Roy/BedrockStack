use core::arch::global_asm;

global_asm!(
    r#"
    .section .text.entry, "ax"
    .globl _start
_start:
    and  rsp, -16
    call entry_main
    mov  rdi, rax           # entry_main status -> exit code
    call exit_process       # write /proc/self:exit, never returns
    hlt
"#
);

#[unsafe(no_mangle)]
pub extern "C" fn exit_process(code: usize) -> ! {
    crate::process::exit(code)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
