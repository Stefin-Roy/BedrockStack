use core::arch::asm;

pub const PIT_CMD: u16 = 0x43;
pub const PIT_DATA0: u16 = 0x40;

pub fn outb(port: u16, val: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags)); }
}

pub fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags)); }
    val
}

pub fn program_one_shot(count: u16) {
    outb(PIT_CMD, 0x30);
    outb(PIT_DATA0, (count & 0xFF) as u8);
    outb(PIT_DATA0, (count >> 8) as u8);
}

pub fn has_fired() -> bool {
    outb(PIT_CMD, 0xE2);
    inb(PIT_DATA0) & 0x80 != 0
}
