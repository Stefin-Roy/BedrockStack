use crate::drivers::serial::SerialPort;
use core::arch::asm;

/// Bit in CPUID EDX[1] indicating APIC presence.
const CPUID_APIC_BIT: u32 = 1 << 9;

/// IA32_APIC_BASE MSR.
const IA32_APIC_BASE_MSR: u32 = 0x1B;

/// Local APIC register offsets (from mapped base).
const LAPIC_SVR: u32 = 0xF0;
const LAPIC_TPR: u32 = 0x80;
const LAPIC_EOI: u32 = 0xB0;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_INIT_COUNT: u32 = 0x380;
const LAPIC_CURR_COUNT: u32 = 0x390;
const LAPIC_DIVIDE_CONFIG: u32 = 0x3E0;

const TIMER_VECTOR: u8 = 32;

// PIT I/O ports.
const PIT_CMD: u16 = 0x43;
const PIT_DATA0: u16 = 0x40;

/// Check whether the CPU supports an on-chip APIC.
fn cpu_has_apic() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    result.edx & CPUID_APIC_BIT != 0
}

fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack));
    }
    (low as u64) | ((high as u64) << 32)
}

fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack));
    }
}

fn outb(port: u16, val: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }
}

fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
    }
    val
}

/// Latched base address of the Local APIC.
static mut LAPIC_BASE: u64 = 0;

fn lapic_write(reg: u32, val: u32) {
    let addr = unsafe { LAPIC_BASE + reg as u64 };
    unsafe {
        (addr as *mut u32).write_volatile(val);
    }
}

fn lapic_read(reg: u32) -> u32 {
    let addr = unsafe { LAPIC_BASE + reg as u64 };
    unsafe { (addr as *const u32).read_volatile() }
}

/// Signal end-of-interrupt to the local APIC.
pub fn apic_eoi() {
    lapic_write(LAPIC_EOI, 0);
}

/// Calibrate the APIC timer using the PIT as a reference.
///
/// Programs PIT channel 0 in one-shot mode with count 0xFFFF, starts the APIC
/// timer at maximum count, waits for the PIT to expire (~54.9 ms), then reads
/// the APIC current count to derive the APIC ticks per 10 ms (100 Hz).
fn calibrate_via_pit() -> u32 {
    SerialPort::puts("[apic] calibrating via PIT\n");

    // Program PIT channel 0: mode 0 (one-shot), write lobyte then hibyte.
    // Command byte = 0x30: channel 0, lobyte+hibyte, mode 0, binary count.
    outb(PIT_CMD, 0x30);
    outb(PIT_DATA0, 0xFF); // low byte  = 0xFF
    outb(PIT_DATA0, 0xFF); // high byte = 0xFF  → total count = 0xFFFF = 65535

    // Start APIC timer: one-shot, unmasked, vector = TIMER_VECTOR.
    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000); // masked initially
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B); // divide by 1
    lapic_write(LAPIC_INIT_COUNT, 0xFFFF_FFFF);
    // Unmask the timer now that it's running.
    let mut lvt = lapic_read(LAPIC_LVT_TIMER);
    lvt &= !0x10000; // clear mask bit
    lapic_write(LAPIC_LVT_TIMER, lvt);

    // Wait for PIT to reach zero by polling the status readback.
    // The PIT count 0xFFFF at 1.193182 MHz gives ~54.925 ms.
    // Timeout after ~200 ms worth of iterations.
    let mut timed_out = true;
    for _ in 0..2_000_000 {
        outb(PIT_CMD, 0xE2); // readback: latch status for channel 0
        let status = inb(PIT_DATA0);
        if status & 0x80 != 0 {
            // Bit 7 = OUT pin (1 = count reached zero).
            timed_out = false;
            break;
        }
    }

    if timed_out {
        SerialPort::puts("[apic] WARN: PIT calibration timed out, using fallback\n");
        // Fallback: assume 100 MHz APIC bus (QEMU default).
        return 1_000_000;
    }

    let current = lapic_read(LAPIC_CURR_COUNT);
    let elapsed = 0xFFFF_FFFFu32.wrapping_sub(current);

    SerialPort::puts("[apic] PIT elapsed APIC ticks: ");
    SerialPort::put_u64(elapsed as u64);
    SerialPort::puts("\n");

    if elapsed == 0 {
        SerialPort::puts("[apic] WARN: zero elapsed ticks, using fallback\n");
        return 1_000_000;
    }

    // PIT period = 65535 / 1193182 seconds ≈ 54.925 ms.
    // Desired period = 10 ms (100 Hz).
    // APIC count = elapsed * (desired / actual)
    //            = elapsed * 10 / (65535 / 1193182 * 1000)
    //            = elapsed * 10 * 1193182 / (65535 * 1000)
    //            = (elapsed as u64) * 11931820 / 65535000
    //            = (elapsed as u64) * 1193182 / 6553500
    let count = ((elapsed as u64) * 1_193_182 / 6_553_500) as u32;

    SerialPort::puts("[apic] calibrated timer count: ");
    SerialPort::put_u64(count as u64);
    SerialPort::puts(" (for 100 Hz)\n");

    if count == 0 {
        SerialPort::puts("[apic] WARN: zero calibrated count, using fallback\n");
        return 1_000_000;
    }

    count
}

/// Initialise the local APIC and start the periodic timer.
pub fn init() {
    if !cpu_has_apic() {
        SerialPort::puts("[apic] FATAL: CPU has no local APIC\n");
        loop {}
    }
    SerialPort::puts("[apic] init\n");

    // Read and verify APIC base address.
    let base = rdmsr(IA32_APIC_BASE_MSR);
    let base_addr = base & 0xFFFF_FFFF_FFFF_F000;
    unsafe { LAPIC_BASE = base_addr; }

    SerialPort::puts("[apic] base=0x");
    SerialPort::put_hex(base_addr);
    SerialPort::puts("\n");

    // Enable the APIC by setting bit 11 in IA32_APIC_BASE.
    if base & (1 << 11) == 0 {
        wrmsr(IA32_APIC_BASE_MSR, base | (1 << 11));
        SerialPort::puts("[apic] enabled via MSR\n");
    }

    // Set Spurious Interrupt Vector Register: bit 8 enables the APIC.
    let svr = lapic_read(LAPIC_SVR);
    lapic_write(LAPIC_SVR, (svr & 0xFFFFFF00) | 0x100 | 0xFF);
    SerialPort::puts("[apic] SVR set\n");

    // Set Task Priority Register to 0 (accept all interrupts).
    lapic_write(LAPIC_TPR, 0);

    // Calibrate the timer and set it to periodic mode.
    let init_count = calibrate_via_pit();

    // Program LVT timer: vector, periodic mode (bit 17 = 1), unmasked.
    let lvt = (TIMER_VECTOR as u32) | (1 << 17);
    lapic_write(LAPIC_LVT_TIMER, lvt);

    // Divide by 1.
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);

    // Set initial count.
    lapic_write(LAPIC_INIT_COUNT, init_count);

    SerialPort::puts("[apic] timer started at 100 Hz\n");
}
