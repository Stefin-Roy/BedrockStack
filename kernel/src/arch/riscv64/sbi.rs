use core::arch::asm;

const SBI_SUCCESS: i64 = 0;
pub const SBI_EXT_LEGACY: u64 = 0x10;
const SBI_EXT_DBCN: u64 = 0x4442434E;
const SBI_EXT_HSM: u64 = 0x48534D;
const SBI_EXT_SRST: u64 = 0x53525354;

const SBI_EXT_LEGACY_CONSOLE_PUTCHAR: u64 = 1;
const SBI_EXT_LEGACY_CONSOLE_GETCHAR: u64 = 2;
const SBI_EXT_LEGACY_SET_TIMER: u64 = 0;
const SBI_EXT_LEGACY_SHUTDOWN: u64 = 8;

const DBCN_WRITE: u64 = 0;
const DBCN_READ: u64 = 1;

fn ecall(extension: u64, function: u64, arg0: u64, arg1: u64, arg2: u64) -> (i64, u64) {
    let error: i64;
    let value: u64;
    unsafe {
        asm!(
            "ecall",
            in("a7") extension,
            in("a6") function,
            in("a0") arg0,
            in("a1") arg1,
            in("a2") arg2,
            lateout("a0") error,
            lateout("a1") value,
            options(nomem, nostack)
        );
    }
    (error, value)
}

pub fn console_putchar(c: u8) {
    ecall(SBI_EXT_LEGACY, SBI_EXT_LEGACY_CONSOLE_PUTCHAR, c as u64, 0, 0);
}

pub fn console_getchar() -> i32 {
    let (_, value) = ecall(SBI_EXT_LEGACY, SBI_EXT_LEGACY_CONSOLE_GETCHAR, 0, 0, 0);
    value as i32
}

pub fn set_timer(stime_value: u64) {
    ecall(SBI_EXT_LEGACY, SBI_EXT_LEGACY_SET_TIMER, stime_value, 0, 0);
}

pub fn shutdown() -> ! {
    ecall(SBI_EXT_LEGACY, SBI_EXT_LEGACY_SHUTDOWN, 0, 0, 0);
    loop { unsafe { asm!("wfi"); } }
}

pub fn probe_extension(extension_id: u64) -> bool {
    let (error, _) = ecall(0x12, 0x12, extension_id, 0, 0);
    error >= 0
}
