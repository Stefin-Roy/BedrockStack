use core::arch::asm;

pub fn read_time() -> u64 {
    let value: u64;
    unsafe { asm!("csrr {}, time", out(reg) value); }
    value
}
