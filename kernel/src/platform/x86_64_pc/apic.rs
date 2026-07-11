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

fn calibrate_via_pit() -> u32 {
    SerialPort::puts("[apic] calibrating via PIT\n");

    pit::program_one_shot(0xFFFF);

    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000);
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, 0xFFFF_FFFF);
    let mut lvt = lapic_read(LAPIC_LVT_TIMER);
    lvt &= !0x10000;
    lapic_write(LAPIC_LVT_TIMER, lvt);

    let mut timed_out = true;
    for _ in 0..2_000_000 {
        if pit::has_fired() { timed_out = false; break; }
    }

    if timed_out {
        SerialPort::puts("[apic] WARN: PIT calibration timed out, using fallback\n");
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

    let lvt = (TIMER_VECTOR as u32) | (1 << 17);
    lapic_write(LAPIC_LVT_TIMER, lvt);
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, init_count);

    SerialPort::puts("[apic] timer started at 100 Hz\n");
}
