use core::ptr::{read_volatile, write_volatile};

const HTIF_BASE: u64 = 0x40008000;
const TOHOST: u64 = 0x0000;
const FROMHOST: u64 = 0x0008;

fn tohost_addr() -> *mut u64 {
    (HTIF_BASE + TOHOST) as *mut u64
}

fn fromhost_addr() -> *mut u64 {
    (HTIF_BASE + FROMHOST) as *mut u64
}

pub fn shutdown() -> ! {
    unsafe {
        write_volatile(tohost_addr(), 0x1000000000000000);
        loop {
            let status = read_volatile(fromhost_addr());
            if status != 0 {
                write_volatile(fromhost_addr(), 0);
                break;
            }
        }
    }
    loop { unsafe { core::arch::asm!("wfi"); } }
}
