use crate::drivers::serial::SerialPort;
use crate::platform::x86_64_pc::pit;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const CPUID_APIC_BIT: u32 = 1 << 9;
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

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    (lo as u64) | ((hi as u64) << 32)
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
        // Higher-half device window: present under the kernel root *and* every
        // cloned task root, so `apic_eoi` works when an IRQ fires on the
        // process CR3 (the low identity window is absent from task roots).
        let addr = crate::mm::layout::LAPIC_VADDR_BASE + reg as u64;
        unsafe {
            (addr as *mut u32).write_volatile(val);
        }
    }
}

fn lapic_read(reg: u32) -> u32 {
    if X2APIC_MODE.load(Ordering::Relaxed) {
        rdmsr(x2apic_msr(reg)) as u32
    } else {
        let addr = crate::mm::layout::LAPIC_VADDR_BASE + reg as u64;
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

/// TSC-backed poll timeout. Replaces the old APIC-counter-based `ApicTimeout`.
///
/// Works at any point after `apic::init()` has finished calibration — no
/// dependency on a running periodic APIC timer.
pub struct PollTimeout {
    deadline_ns: u64,
}

impl PollTimeout {
    pub fn new(ms: u64) -> Self {
        Self {
            deadline_ns: tsc_now_ns() + ms * 1_000_000,
        }
    }

    pub fn expired(&self) -> bool {
        tsc_now_ns() >= self.deadline_ns
    }
}

/// Returns the calibrated TSC frequency in Hz.
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Returns the TSC value captured at boot calibration.
pub fn tsc_boot() -> u64 {
    TSC_BOOT.load(Ordering::Relaxed)
}

/// Returns the calibrated APIC frequency in Hz.
pub fn apic_hz() -> u64 {
    APIC_HZ.load(Ordering::Relaxed)
}

/// Read TSC and convert to nanoseconds since boot.
///
/// Uses divide-first arithmetic to avoid u64 overflow (the naive
/// `delta * 1_000_000_000 / hz` overflows after ~9 s of uptime).
/// Returns 0 before calibration.
pub fn tsc_now_ns() -> u64 {
    let hz = TSC_HZ.load(Ordering::Relaxed);
    if hz == 0 {
        return 0;
    }
    let boot = TSC_BOOT.load(Ordering::Relaxed);
    let now = rdtsc();
    let delta = now.wrapping_sub(boot);

    //  delta / hz  → whole seconds
    //  delta % hz  → remainder (sub-second ticks)
    let secs = delta / hz;
    let remainder = delta % hz;

    let ns = secs * 1_000_000_000 + (remainder * 1_000_000_000) / hz;
    ns
}

/// Program the LAPIC timer for a one-shot interrupt after `count` ticks.
///
/// The timer fires once on vector `TIMER_VECTOR`, then stops until
/// reprogrammed.  Call `timer_stop()` to cancel a pending shot.
pub fn oneshot_timer_set(count: u32) {
    lapic_write(LAPIC_LVT_TIMER, TIMER_VECTOR as u32);
    lapic_write(LAPIC_INIT_COUNT, count);
}

/// Mask (stop) the LAPIC timer.  No further interrupts will fire until
/// `oneshot_timer_set()` is called again.
pub fn timer_stop() {
    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000);
    lapic_write(LAPIC_INIT_COUNT, 0);
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
pub const IPI_TIMER: u8 = 52;

pub fn send_resched(cpu_id: u8) {
    send_ipi(
        crate::smp::per_cpu_by_id(cpu_id as u32).apic_id,
        IPI_RESCHEDULE,
    );
}

pub fn send_tlb_shootdown(cpu_id: u8) {
    send_ipi(cpu_id as u32, IPI_TLB_SHOOTDOWN);
}

const PIT_HZ: u64 = 1_193_182;
const PIT_RELOAD: u64 = 0xFFFF;
pub const TIMER_HZ: u64 = 1000;
pub const TIMER_PERIOD_MS: u32 = (1000 / TIMER_HZ) as u32;

/// Calibrated APIC timer count shared between BSP and APs.
pub(crate) static BSP_TIMER_COUNT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Calibrated TSC frequency in Hz.
pub(crate) static TSC_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// TSC value captured at boot (during calibration).
pub(crate) static TSC_BOOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Calibrated APIC timer frequency in Hz.
pub(crate) static APIC_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn calibrate_via_pit() -> u32 {
    SerialPort::puts("[apic] calibrating via PIT\n");

    // Program the PIT for a one-shot interval.
    pit::program_one_shot(PIT_RELOAD as u16);

    // Start the LAPIC timer in one-shot mode with the largest possible count.
    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000); // masked
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, 0xFFFF_FFFF);

    // Sample TSC right when the APIC counter starts.
    let tsc_start = rdtsc();

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

    // Sample TSC at PIT expiry.
    let tsc_end = rdtsc();

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
    let apic_hz_val = elapsed * PIT_HZ / PIT_RELOAD;

    SerialPort::puts("[apic] estimated APIC frequency: ");
    SerialPort::put_u64(apic_hz_val);
    SerialPort::puts(" Hz\n");

    APIC_HZ.store(apic_hz_val, core::sync::atomic::Ordering::Relaxed);

    // TSC frequency (Hz) from the same PIT interval.
    let tsc_elapsed = tsc_end.wrapping_sub(tsc_start);
    if tsc_elapsed > 0 {
        let tsc_hz_val = tsc_elapsed * PIT_HZ / PIT_RELOAD;
        TSC_HZ.store(tsc_hz_val, core::sync::atomic::Ordering::Relaxed);
        TSC_BOOT.store(tsc_start, core::sync::atomic::Ordering::Relaxed);
        SerialPort::puts("[apic] estimated TSC frequency: ");
        SerialPort::put_u64(tsc_hz_val);
        SerialPort::puts(" Hz\n");
    } else {
        SerialPort::puts("[apic] WARN: TSC elapsed zero, using APIC-based clocksource\n");
    }

    // Initial LAPIC count for the requested interrupt frequency.
    let count = (apic_hz_val / TIMER_HZ) as u32;

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

    // Don't write IA32_APIC_BASE MSR on APs — it may cause #GP on many CPUs
    // (Intel SDM: "Writes from a logical processor other than the BSP ... may
    // cause a general-protection exception").  The BSP already set up APIC
    // enable and forced xAPIC mode globally.
    X2APIC_MODE.store(false, Ordering::Relaxed);

    let svr = lapic_read(LAPIC_SVR);
    lapic_write(LAPIC_SVR, (svr & 0xFFFFFF00) | 0x100 | 0xFF);

    lapic_write(LAPIC_TPR, 0);

    // AP LAPIC timer: masked one-shot, ready but never fires until armed.
    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000);
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, 0);

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

    // Force xAPIC mode — disable x2APIC.
    // QEMU TCG sets CPUID x2APIC bit and MSR 0x1B bit 10, but the x2APIC MSR
    // range causes #GP anyway.  On real hardware xAPIC MMIO is always available
    // as a fallback and the performance difference is negligible for this kernel.
    let cur = rdmsr(IA32_APIC_BASE_MSR);
    wrmsr(IA32_APIC_BASE_MSR, cur & !(1 << 10));
    X2APIC_MODE.store(false, Ordering::Relaxed);
    SerialPort::puts("[apic] x2APIC mode: 0 (forced to xAPIC)\n");

    let svr = lapic_read(LAPIC_SVR);
    lapic_write(LAPIC_SVR, (svr & 0xFFFFFF00) | 0x100 | 0xFF);
    SerialPort::puts("[apic] SVR set\n");

    lapic_write(LAPIC_TPR, 0);

    let init_count = calibrate_via_pit();
    BSP_TIMER_COUNT.store(init_count, core::sync::atomic::Ordering::Relaxed);

    // Leave timer in masked one-shot mode.  The UniversalTimer's clockevent
    // will arm it when the first one-shot deadline is set.
    lapic_write(LAPIC_LVT_TIMER, (TIMER_VECTOR as u32) | 0x10000);
    lapic_write(LAPIC_DIVIDE_CONFIG, 0x0B);
    lapic_write(LAPIC_INIT_COUNT, 0);

    SerialPort::puts("[apic] timer calibrated, waiting for first deadline\n");
}
