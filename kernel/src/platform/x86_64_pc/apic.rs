use crate::drivers::serial::SerialPort;
use crate::platform::x86_64_pc::pit;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const CPUID_APIC_BIT: u32 = 1 << 9;
const CPUID_X2APIC_BIT: u32 = 1 << 21;
const IA32_APIC_BASE_MSR: u32 = 0x1B;
const IA32_X2APIC_ID_MSR: u32 = 0x802;
const IA32_X2APIC_ICR_MSR: u32 = 0x830;

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

/// Set when the local APIC is operating in x2APIC mode (IA32_APIC_BASE[10]).
static X2APIC_MODE: AtomicBool = AtomicBool::new(false);

fn cpu_has_apic() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    result.edx & CPUID_APIC_BIT != 0
}

fn cpu_has_x2apic() -> bool {
    let result = core::arch::x86_64::__cpuid(1);
    result.ecx & CPUID_X2APIC_BIT != 0
}

fn is_x2apic_enabled() -> bool {
    unsafe {
        let base = rdmsr(IA32_APIC_BASE_MSR);
        base & (1 << 10) != 0
    }
}

/// Send an IPI via the appropriate path for the current APIC mode.
///
/// In xAPIC mode the destination is written to ICR high bits 31:24 (8-bit
/// destination field). In x2APIC mode the full 32-bit ID is written to the
/// ICR MSR directly.
fn send_ipi_raw(dest_apic_id: u32, icr_low: u32) {
    unsafe {
        if X2APIC_MODE.load(Ordering::Relaxed) {
            let icr = ((dest_apic_id as u64) << 32) | (icr_low as u64);
            wrmsr(IA32_X2APIC_ICR_MSR, icr);
        } else {
            // Wait for previous IPI to complete (delivery status bit = 0)
            while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
                core::hint::spin_loop();
            }
            lapic_write(LAPIC_ICR_HIGH, (dest_apic_id & 0xFF) << 24);
            lapic_write(LAPIC_ICR_LOW, icr_low);
        }
    }
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

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Map an xAPIC MMIO register offset to its x2APIC MSR index.
///
/// x2APIC registers live at MSR `0x800 + (offset >> 4)` (e.g. SVR 0xF0 -> 0x80F).
fn x2apic_msr(reg: u32) -> u32 {
    0x800 + (reg >> 4)
}

fn lapic_write(reg: u32, val: u32) {
    if X2APIC_MODE.load(Ordering::Relaxed) {
        wrmsr(x2apic_msr(reg), val as u64);
    } else {
        let addr = LAPIC_BASE.load(Ordering::Relaxed) + reg as u64;
        unsafe { (addr as *mut u32).write_volatile(val); }
    }
}

fn lapic_read(reg: u32) -> u32 {
    if X2APIC_MODE.load(Ordering::Relaxed) {
        rdmsr(x2apic_msr(reg)) as u32
    } else {
        let addr = LAPIC_BASE.load(Ordering::Relaxed) + reg as u64;
        unsafe { (addr as *const u32).read_volatile() }
    }
}

pub fn apic_eoi() {
    lapic_write(LAPIC_EOI, 0);
}

/// Returns the current LAPIC timer count (decrements from init_count to 0).
/// The timer fires every ~10ms, reloading init_count each period.
pub fn timer_current_count() -> u32 {
    lapic_read(LAPIC_CURR_COUNT)
}

/// Returns the initial LAPIC timer count loaded each period.
pub fn timer_init_count() -> u32 {
    BSP_TIMER_COUNT.load(Ordering::Relaxed)
}

pub fn read_apic_id() -> u8 {
    (lapic_read(LAPIC_ID) >> 24) as u8
}

pub fn read_x2apic_id() -> u32 {
    rdmsr(IA32_X2APIC_ID_MSR) as u32
}

/// Read the current CPU's APIC ID as a 32-bit value, working in both xAPIC
/// and x2APIC modes.
pub fn read_full_apic_id() -> u32 {
    if X2APIC_MODE.load(Ordering::Relaxed) {
        read_x2apic_id()
    } else {
        (lapic_read(LAPIC_ID) >> 24) as u32
    }
}

pub fn lapic_base() -> u64 {
    LAPIC_BASE.load(Ordering::Relaxed)
}

/// Send a fixed IPI to a specific APIC ID.
pub fn send_ipi(dest_apic_id: u32, vector: u8) {
    // delivery mode = 000 (fixed), assert, edge trigger, physical destination
    send_ipi_raw(dest_apic_id, vector as u32);
}

/// Send INIT IPI to a specific APIC ID (assert).
pub fn send_init_ipi(dest_apic_id: u32) {
    // delivery mode = 101 (INIT), level = 1, trigger mode = 1 (level)
    send_ipi_raw(dest_apic_id, (5 << 8) | (1 << 14) | (1 << 15));
}

/// Send INIT de-assert IPI to a specific APIC ID.
///
/// Completes the INIT-INIT-SIPI sequence required by the MP specification.
pub fn send_init_deassert(dest_apic_id: u32) {
    // delivery mode = 101 (INIT), level = 0, trigger mode = 1 (level)
    send_ipi_raw(dest_apic_id, (5 << 8) | (0 << 14) | (1 << 15));
}

/// Send SIPI (Startup IPI) to a specific APIC ID.
///
/// `page` is the 4K-aligned physical address >> 12 of the trampoline code.
pub fn send_sipi_ipi(dest_apic_id: u32, page: u8) {
    // delivery mode = 110 (SIPI), vector = page
    send_ipi_raw(dest_apic_id, (6 << 8) | (page as u32));
}

/// Send IPI to all CPUs except self (broadcast to all-but-self).
pub fn send_ipi_all_except_self(vector: u8) {
    unsafe {
        if X2APIC_MODE.load(Ordering::Relaxed) {
            // x2APIC shorthand lives in ICR bits 18:16. Delivery mode 000
            // (fixed) is implicit. destination shorthand = 11 (all excluding self)
            let icr = (vector as u64) | (3 << 18);
            wrmsr(IA32_X2APIC_ICR_MSR, icr);
        } else {
            while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
                core::hint::spin_loop();
            }
            // destination shorthand = 10 (all except self)
            lapic_write(LAPIC_ICR_HIGH, 0);
            lapic_write(LAPIC_ICR_LOW, (3 << 18) | (vector as u32));
        }
    }
}

pub const IPI_RESCHEDULE: u8 = 49;
pub const IPI_TLB_SHOOTDOWN: u8 = 50;
pub const IPI_HALT: u8 = 51;

pub fn send_resched(cpu_id: u8) {
    send_ipi(cpu_id as u32, IPI_RESCHEDULE);
}

pub fn send_tlb_shootdown(cpu_id: u8) {
    send_ipi(cpu_id as u32, IPI_TLB_SHOOTDOWN);
}

const PIT_HZ: u64 = 1_193_182;
const PIT_RELOAD: u64 = 0xFFFF;
pub const TIMER_HZ: u64 = 1000;
pub const TIMER_PERIOD_MS: u32 = (1000 / TIMER_HZ) as u32;

/// Calibrated APIC timer count shared between BSP and APs.
pub(crate) static BSP_TIMER_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

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
    LAPIC_BASE.store(base_addr, Ordering::Relaxed);

    X2APIC_MODE.store(is_x2apic_enabled(), Ordering::Relaxed);

    if base & (1 << 11) == 0 {
        wrmsr(IA32_APIC_BASE_MSR, base | (1 << 11));
    }

    // x2APIC enable is per-logical-processor; the BSP enabled it for itself,
    // but each AP must enable it on its own local APIC.
    if cpu_has_x2apic() && !is_x2apic_enabled() {
        let cur = rdmsr(IA32_APIC_BASE_MSR);
        wrmsr(IA32_APIC_BASE_MSR, cur | (1 << 10));
    }
    X2APIC_MODE.store(is_x2apic_enabled(), Ordering::Relaxed);

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
    LAPIC_BASE.store(base_addr, Ordering::Relaxed);

    SerialPort::puts("[apic] base=0x");
    SerialPort::put_hex(base_addr);
    SerialPort::puts("\n");

    if base & (1 << 11) == 0 {
        wrmsr(IA32_APIC_BASE_MSR, base | (1 << 11));
        SerialPort::puts("[apic] enabled via MSR\n");
    }

    // Enable x2APIC mode when the CPU supports it. This makes ICR accesses and
    // APIC ID reads use MSRs, which is required for APIC IDs wider than 8 bits.
    if cpu_has_x2apic() && !is_x2apic_enabled() {
        let cur = rdmsr(IA32_APIC_BASE_MSR);
        wrmsr(IA32_APIC_BASE_MSR, cur | (1 << 10));
        SerialPort::puts("[apic] x2APIC mode enabled\n");
    }
    X2APIC_MODE.store(is_x2apic_enabled(), Ordering::Relaxed);
    SerialPort::puts("[apic] x2APIC mode: ");
    SerialPort::put_u64(if X2APIC_MODE.load(Ordering::Relaxed) { 1 } else { 0 });
    SerialPort::puts("\n");

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

    SerialPort::puts("[apic] timer started at 1000 Hz\n");
}
