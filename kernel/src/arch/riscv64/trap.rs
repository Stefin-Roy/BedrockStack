use core::arch::asm;

const SCAUSE_INTERRUPT: u64 = 1 << 63;

const SUPV_SOFTWARE: u64 = 1;
const SUPV_TIMER: u64 = 5;
const SUPV_EXTERNAL: u64 = 9;

pub const MIE_SSIE: u64 = 1 << 1;
pub const MIE_STIE: u64 = 1 << 5;
pub const MIE_SEIE: u64 = 1 << 9;

core::arch::global_asm!(
    ".align 4",
    "__trap_entry:",
    "addi sp, sp, -256",
    "sd ra, 0x00(sp)",
    "sd gp, 0x08(sp)",
    "sd tp, 0x10(sp)",
    "sd t0, 0x18(sp)",
    "sd t1, 0x20(sp)",
    "sd t2, 0x28(sp)",
    "sd s0, 0x30(sp)",
    "sd s1, 0x38(sp)",
    "sd a0, 0x40(sp)",
    "sd a1, 0x48(sp)",
    "sd a2, 0x50(sp)",
    "sd a3, 0x58(sp)",
    "sd a4, 0x60(sp)",
    "sd a5, 0x68(sp)",
    "sd a6, 0x70(sp)",
    "sd a7, 0x78(sp)",
    "sd s2, 0x80(sp)",
    "sd s3, 0x88(sp)",
    "sd s4, 0x90(sp)",
    "sd s5, 0x98(sp)",
    "sd s6, 0xa0(sp)",
    "sd s7, 0xa8(sp)",
    "sd s8, 0xb0(sp)",
    "sd s9, 0xb8(sp)",
    "sd s10, 0xc0(sp)",
    "sd s11, 0xc8(sp)",
    "sd t3, 0xd0(sp)",
    "sd t4, 0xd8(sp)",
    "sd t5, 0xe0(sp)",
    "sd t6, 0xe8(sp)",
    "csrr t0, sepc",
    "sd t0, 0xf0(sp)",
    "csrr t0, sstatus",
    "sd t0, 0xf8(sp)",
    "mv a0, sp",
    "call __trap_handler",
    "ld t0, 0xf0(sp)",
    "csrw sepc, t0",
    "ld t0, 0xf8(sp)",
    "csrw sstatus, t0",
    "ld ra, 0x00(sp)",
    "ld gp, 0x08(sp)",
    "ld tp, 0x10(sp)",
    "ld t0, 0x18(sp)",
    "ld t1, 0x20(sp)",
    "ld t2, 0x28(sp)",
    "ld s0, 0x30(sp)",
    "ld s1, 0x38(sp)",
    "ld a0, 0x40(sp)",
    "ld a1, 0x48(sp)",
    "ld a2, 0x50(sp)",
    "ld a3, 0x58(sp)",
    "ld a4, 0x60(sp)",
    "ld a5, 0x68(sp)",
    "ld a6, 0x70(sp)",
    "ld a7, 0x78(sp)",
    "ld s2, 0x80(sp)",
    "ld s3, 0x88(sp)",
    "ld s4, 0x90(sp)",
    "ld s5, 0x98(sp)",
    "ld s6, 0xa0(sp)",
    "ld s7, 0xa8(sp)",
    "ld s8, 0xb0(sp)",
    "ld s9, 0xb8(sp)",
    "ld s10, 0xc0(sp)",
    "ld s11, 0xc8(sp)",
    "ld t3, 0xd0(sp)",
    "ld t4, 0xd8(sp)",
    "ld t5, 0xe0(sp)",
    "ld t6, 0xe8(sp)",
    "addi sp, sp, 256",
    "sret",
);

unsafe extern "C" {
    fn __trap_entry();
}

#[unsafe(no_mangle)]
extern "C" fn __trap_handler(frame: &TrapFrame) {
    let scause: u64;
    let stval: u64;
    unsafe {
        asm!("csrr {}, scause", out(reg) scause);
        asm!("csrr {}, stval", out(reg) stval);
    }

    if scause & SCAUSE_INTERRUPT != 0 {
        let interrupt = scause & !SCAUSE_INTERRUPT;
        match interrupt {
            SUPV_TIMER => {}
            SUPV_EXTERNAL => {}
            SUPV_SOFTWARE => {}
            _ => {
                panic!("unhandled interrupt: scause={:#x}, sepc={:#x}", scause, frame.sepc);
            }
        }
    } else {
        let exception = scause;
        match exception {
            12 | 13 | 15 => {
                panic!("page fault: scause={:#x}, sepc={:#x}, stval={:#x}", scause, frame.sepc, stval);
            }
            2 => {
                panic!("illegal instruction: sepc={:#x}", frame.sepc);
            }
            _ => {
                panic!("unhandled exception: scause={:#x}, sepc={:#x}, stval={:#x}", scause, frame.sepc, stval);
            }
        }
    }
}

pub fn init() {
    unsafe {
        asm!("csrw stvec, {}", in(reg) __trap_entry as *const () as u64);
    }
}

#[repr(C)]
struct TrapFrame {
    ra: u64,
    gp: u64,
    tp: u64,
    t0: u64,
    t1: u64,
    t2: u64,
    s0: u64,
    s1: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
    a7: u64,
    s2: u64,
    s3: u64,
    s4: u64,
    s5: u64,
    s6: u64,
    s7: u64,
    s8: u64,
    s9: u64,
    s10: u64,
    s11: u64,
    t3: u64,
    t4: u64,
    t5: u64,
    t6: u64,
    sepc: u64,
    sstatus: u64,
}
