//! COM1 (16550 UART) serial driver using raw port I/O.
//!
//! Shared by the bootloader and the kernel. Works both under UEFI boot
//! services and on bare metal after `exit_boot_services`, because it only
//! uses `in`/`out` port instructions.

use core::fmt;

pub struct SerialPort;

impl SerialPort {
    const PORT: u16 = 0x3F8;

    fn outb(port: u16, val: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
        }
    }

    fn inb(port: u16) -> u8 {
        let val: u8;
        unsafe {
            core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
        }
        val
    }

    /// Initialize COM1 at 115200 baud, 8N1.
    pub fn init() {
        Self::outb(Self::PORT + 1, 0x00); // disable interrupts
        Self::outb(Self::PORT + 3, 0x80); // enable DLAB (set baud divisor)
        // Divisor = 1 -> 115200 / 1 = 115200 baud. (low byte, high byte)
        Self::outb(Self::PORT, 0x01);
        Self::outb(Self::PORT + 1, 0x00);
        Self::outb(Self::PORT + 3, 0x03); // 8 bits, no parity, 1 stop bit; DLAB off
        Self::outb(Self::PORT + 2, 0xC7); // enable + clear FIFO, 14-byte threshold
        Self::outb(Self::PORT + 4, 0x0B); // IRQs enabled, RTS/DSR set
    }

    pub fn putc(c: u8) {
        // Spin with a timeout: if the transmitter stays busy for ~100K
        // iterations, write anyway (best-effort) to avoid hanging the kernel.
        for _ in 0..100_000 {
            if Self::inb(Self::PORT + 5) & 0x20 != 0 {
                Self::outb(Self::PORT, c);
                return;
            }
            core::hint::spin_loop();
        }
        // Timeout — write anyway; data may be lost but the kernel continues.
        Self::outb(Self::PORT, c);
    }

    pub fn puts(s: &str) {
        for b in s.bytes() {
            if b == b'\n' {
                Self::putc(b'\r');
            }
            Self::putc(b);
        }
    }

    pub fn put_hex(mut val: u64) {
        if val == 0 {
            Self::puts("0");
            return;
        }
        let mut buf = [0u8; 16];
        let mut i = 16;
        while val > 0 {
            i -= 1;
            let digit = (val & 0xF) as u8;
            buf[i] = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            val >>= 4;
        }
        Self::puts(core::str::from_utf8(&buf[i..]).unwrap_or("???"));
    }

    pub fn put_u64(mut val: u64) {
        if val == 0 {
            Self::puts("0");
            return;
        }
        let mut buf = [0u8; 20];
        let mut i = 20;
        while val > 0 {
            i -= 1;
            buf[i] = b'0' + (val % 10) as u8;
            val /= 10;
        }
        Self::puts(core::str::from_utf8(&buf[i..]).unwrap_or("???"));
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Self::puts(s);
        Ok(())
    }
}
