use core::fmt;

/// Abstraction over hardware I/O for a 16550 UART.
///
/// On x86 this is port-mapped I/O (`in`/`out`); on RISC-V it is MMIO at a
/// fixed base address. The trait uses register *offsets* (0..=7) so the same
/// `SerialPort` code works regardless of the addressing scheme.
pub trait IoBackend {
    fn read_reg(offset: u16) -> u8;
    fn write_reg(offset: u16, val: u8);
}

/// 16550 UART serial driver parameterised by an I/O backend.
pub struct SerialPort<B: IoBackend> {
    _phantom: core::marker::PhantomData<B>,
}

impl<B: IoBackend> fmt::Write for SerialPort<B> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Self::puts(s);
        Ok(())
    }
}

impl<B: IoBackend> SerialPort<B> {
    pub fn new() -> Self {
        Self { _phantom: core::marker::PhantomData }
    }

    /// Initialize COM1 at 115200 baud, 8N1.
    pub fn init() {
        B::write_reg(1, 0x00); // disable interrupts
        B::write_reg(3, 0x80); // enable DLAB (set baud divisor)
        B::write_reg(0, 0x01); // divisor = 1 → 115200 baud
        B::write_reg(1, 0x00);
        B::write_reg(3, 0x03); // 8 bits, no parity, 1 stop bit; DLAB off
        B::write_reg(2, 0xC7); // enable + clear FIFO, 14-byte threshold
        B::write_reg(4, 0x0B); // IRQs enabled, RTS/DSR set
    }

    pub fn putc(c: u8) {
        // Spin with a timeout: if the transmitter stays busy for ~100K
        // iterations, write anyway (best-effort) to avoid hanging the kernel.
        for _ in 0..100_000 {
            if B::read_reg(5) & 0x20 != 0 {
                B::write_reg(0, c);
                return;
            }
            core::hint::spin_loop();
        }
        // Timeout — write anyway; data may be lost but the kernel continues.
        B::write_reg(0, c);
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

// ---------------------------------------------------------------------------
// x86_64 port-mapped I/O backend (available when compiling for x86_64).
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
pub mod x86_64 {
    use super::IoBackend;

    pub struct PortIo;

    impl IoBackend for PortIo {
        fn read_reg(offset: u16) -> u8 {
            let port = 0x3F8 + offset;
            let val: u8;
            unsafe {
                core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
            }
            val
        }

        fn write_reg(offset: u16, val: u8) {
            let port = 0x3F8 + offset;
            unsafe {
                core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// Concrete serial port type for x86_64 (port-mapped I/O at 0x3F8).
    pub type SerialPort = super::SerialPort<PortIo>;
}
