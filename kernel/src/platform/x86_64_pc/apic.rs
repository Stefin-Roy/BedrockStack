use crate::drivers::serial::SerialPort;
use crate::platform::x86_64_pc::pit;
use core::arch::asm;

const CPUID_APIC_BIT: u32 = 1 << 9;
const IA32_APIC_BASE_MSR: u32 = 0x1B;

const LAPIC_SVR: u32 = 0xF0;
const LAPIC_TPR: u32 = 0x80;
const LAPIC_EOI: u32 = 0xB0;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_INIT_COUNT: u32 = 0x380;
const LAPIC_CURR_COUNT: u32 = 0x390;
const LAPIC_DIVIDE_CONFIG: u32 = 0x3E0;
const LAPIC_ID: u32 = 0x20;
const LAPIC_ICR_LOW: u32 = 0x300;
const LAPIC_ICR_HIGH: u32 = 0x310;

const TIMER_VECTOR: u8 = 32;

fn cpu_has_apic() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    result.edx & CPUID_APIC_BIT != 0
}

fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack)); }
    (low as u64) | ((high as u64) << 32)
}

fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack)); }
}

static mut LAPIC_BASE: u64 = 0;

fn lapic_write(reg: u32, val: u32) {
    let addr = unsafe { LAPIC_BASE + reg as u64 };
    unsafe { (addr as *mut u32).write_volatile(val); }
}

fn lapic_read(reg: u32) -> u32 {
    let addr = unsafe { LAPIC_BASE + reg as u64 };
    unsafe { (addr as *const u32).read_volatile() }
}

pub fn apic_eoi() {
    lapic_write(LAPIC_EOI, 0);
}
pub fn read_apic_id() -> u8 {
    (lapic_read(LAPIC_ID) >> 24) as u8
}

pub fn lapic_base() -> u64 {
    unsafe { LAPIC_BASE }
}

/// Send a fixed IPI to a specific APIC ID.
pub fn send_ipi(dest_apic_id: u8, vector: u8) {
    // Wait for previous IPI to complete (delivery status bit = 0)
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
    // Write destination APIC ID to ICR high
    lapic_write(LAPIC_ICR_HIGH, (dest_apic_id as u32) << 24);
    // Write vector + delivery mode (fixed = 000) + assert
    lapic_write(LAPIC_ICR_LOW, vector as u32);
}

/// Send INIT IPI to a specific APIC ID (assert).
pub fn send_init_ipi(dest_apic_id: u8) {
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
    lapic_write(LAPIC_ICR_HIGH, (dest_apic_id as u32) << 24);
    // delivery mode = 101 (INIT), level = 1, trigger mode = 1 (level)
    lapic_write(LAPIC_ICR_LOW, (5 << 8) | (1 << 14) | (1 << 15));
}

/// Send INIT de-assert IPI to a specific APIC ID.
///
/// Completes the INIT-INIT-SIPI sequence required by the MP specification.
pub fn send_init_deassert(dest_apic_id: u8) {
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
    lapic_write(LAPIC_ICR_HIGH, (dest_apic_id as u32) << 24);
    // delivery mode = 101 (INIT), level = 0, trigger mode = 1 (level)
    lapic_write(LAPIC_ICR_LOW, (5 << 8) | (0 << 14) | (1 << 15));
}

/// Send SIPI (Startup IPI) to a specific APIC ID.
///
/// `page` is the 4K-aligned physical address >> 12 of the trampoline code.
pub fn send_sipi_ipi(dest_apic_id: u8, page: u8) {
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
    lapic_write(LAPIC_ICR_HIGH, (dest_apic_id as u32) << 24);
    // delivery mode = 110 (SIPI), vector = page
    lapic_write(LAPIC_ICR_LOW, (6 << 8) | (page as u32));
}

/// Send IPI to all CPUs except self (broadcast to all-but-self).
pub fn send_ipi_all_except_self(vector: u8) {
    while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
    // destination shorthand = 10 (all except self)
    lapic_write(LAPIC_ICR_HIGH, 0);
    lapic_write(LAPIC_ICR_LOW, (3 << 18) | (vector as u32));
}

pub const IPI_RESCHEDULE: u8 = 49;
pub const IPI_TLB_SHOOTDOWN: u8 = 50;
pub const IPI_HALT: u8 = 51;

pub fn send_resched(cpu_id: u8) {
    send_ipi(cpu_id, IPI_RESCHEDULE);
}

pub fn send_tlb_shootdown(cpu_id: u8) {
    send_ipi(cpu_id, IPI_TLB_SHOOTDOWN);
}

const PIT_HZ: u64 = 1_193_182;
const PIT_RELOAD: u64 = 0xFFFF;
const TIMER_HZ: u64 = 100; // Change to 1000 for a 1 kHz timer.

/// Calibrated APIC timer count shared between BSP and APs.
static BSP_TIMER_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn calibrate_via_pit() -> u32 {
    SerialPort::puts("[apic] calibrating via PIT\n");

    // Program the PIT for a one-shot interval.
    pit::program_one_shot(PIT_RELOAD as u16);

    // Start the LAPIC timer in one-shot mode with the largest possible count.
    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000); // masked
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, 0xFFFF_FFFF);

    // Unmask and let it begin counting.
    let mut lvt = lapic_read(LAPIC_LVT_TIMER);
    lvt &= !0x10000;
    lapic_write(LAPIC_LVT_TIMER, lvt);

    // Wait for the PIT to expire.
    let mut timed_out = true;
    for _ in 0..2_000_000 {
        if pit::has_fired() {
            timed_out = false;
            break;
        }
    }

    if timed_out {
        SerialPort::puts("[apic] WARN: PIT calibration timed out, using fallback\n");
        return 1_000_000;
    }

    // Number of LAPIC ticks during the PIT interval.
    let current = lapic_read(LAPIC_CURR_COUNT);
    let elapsed = 0xFFFF_FFFFu32.wrapping_sub(current) as u64;

    SerialPort::puts("[apic] PIT elapsed APIC ticks: ");
    SerialPort::put_u64(elapsed);
    SerialPort::puts("\n");

    if elapsed == 0 {
        SerialPort::puts("[apic] WARN: zero elapsed ticks, using fallback\n");
        return 1_000_000;
    }

    // APIC frequency (Hz):
    //
    //   elapsed_ticks
    //   -------------  * PIT_HZ
    //    PIT_RELOAD
    //
    let apic_hz = elapsed * PIT_HZ / PIT_RELOAD;

    SerialPort::puts("[apic] estimated APIC frequency: ");
    SerialPort::put_u64(apic_hz);
    SerialPort::puts(" Hz\n");

    // Initial LAPIC count for the requested interrupt frequency.
    let count = (apic_hz / TIMER_HZ) as u32;

    SerialPort::puts("[apic] calibrated timer count: ");
    SerialPort::put_u64(count as u64);
    SerialPort::puts(" (for ");
    SerialPort::put_u64(TIMER_HZ);
    SerialPort::puts(" Hz)\n");

    if count == 0 {
        SerialPort::puts("[apic] WARN: zero calibrated count, using fallback\n");
        return 1_000_000;
    }

    count
}
/// Initialize the LAPIC on an AP (secondary CPU).
///
/// Called once per AP during startup. Skips PIT calibration (the timer is
/// already running from BSP init).
pub fn init_ap() {
    SerialPort::puts("[apic] AP init\n");
    let base = rdmsr(IA32_APIC_BASE_MSR);
    let base_addr = base & 0xFFFF_FFFF_FFFF_F000;
    unsafe { LAPIC_BASE = base_addr; }

    if base & (1 << 11) == 0 {
        wrmsr(IA32_APIC_BASE_MSR, base | (1 << 11));
    }

    let svr = lapic_read(LAPIC_SVR);
    lapic_write(LAPIC_SVR, (svr & 0xFFFFFF00) | 0x100 | 0xFF);

    lapic_write(LAPIC_TPR, 0);

    // AP LAPIC timer: use the calibrated count from BSP (same frequency).
    let lvt = (TIMER_VECTOR as u32) | (1 << 17);
    lapic_write(LAPIC_LVT_TIMER, lvt);
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, BSP_TIMER_COUNT.load(core::sync::atomic::Ordering::Relaxed));

    SerialPort::puts("[apic] AP init done\n");
}

pub fn init() {
    if !cpu_has_apic() {
        SerialPort::puts("[apic] FATAL: CPU has no local APIC\n");
        loop {}
    }
    SerialPort::puts("[apic] init\n");

    let base = rdmsr(IA32_APIC_BASE_MSR);
    let base_addr = base & 0xFFFF_FFFF_FFFF_F000;
    unsafe { LAPIC_BASE = base_addr; }

    SerialPort::puts("[apic] base=0x");
    SerialPort::put_hex(base_addr);
    SerialPort::puts("\n");

    if base & (1 << 11) == 0 {
        wrmsr(IA32_APIC_BASE_MSR, base | (1 << 11));
        SerialPort::puts("[apic] enabled via MSR\n");
    }

    let svr = lapic_read(LAPIC_SVR);
    lapic_write(LAPIC_SVR, (svr & 0xFFFFFF00) | 0x100 | 0xFF);
    SerialPort::puts("[apic] SVR set\n");

    lapic_write(LAPIC_TPR, 0);

    let init_count = calibrate_via_pit();
    BSP_TIMER_COUNT.store(init_count, core::sync::atomic::Ordering::Relaxed);

    let lvt = (TIMER_VECTOR as u32) | (1 << 17);
    lapic_write(LAPIC_LVT_TIMER, lvt);
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, init_count);

    SerialPort::puts("[apic] timer started at 100 Hz\n");
}
