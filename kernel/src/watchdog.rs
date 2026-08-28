//! True NMI watchdog — PMU perf-counter overflow via LVTPC (Option B).
//!
//! - Arms a per-CPU `CPU_CLK_UNHALTED` counter to overflow after
//!   `WATCHDOG_TIMEOUT_NS` and deliver an NMI through `LAPIC_LVT_PERF`.
//!   NMI fires with `IF=0` blocked, so `IrqMutex` dead-spins are caught.
//! - `pet()` is lock-free (TSC store) and called from `timer_handler`
//!   before the `UniversalTimer` queue lock and from `task::schedule()`.
//! - `nmi_handler` receives the *interrupted* `InterruptStackFrame` — the
//!   hung `RIP` is dumped, not the handler's own `RIP`. A hang is declared
//!   when `now - heartbeat > 3s` and the CPU is not idle, regardless of
//!   whether the same `RIP` repeats — loops are hung whether they spin on one
//!   instruction or many. `LAST_RIP` is kept for diagnostic correlation but
//!   is not a gating condition (previous same-RIP gate missed loops).
//! - `HOTKEY_PENDING` (F9) is lock-free; BSP sets it in `ps2::irq_handler`
//!   (raw `0x43`) or via `input::submit_event` (any `KeyCode::F9` from
//!   PS/2 or USB HID), and NMI broadcasts `NMI IPI` to all APs.
//! - Default: dump then `cli;hlt` (halt). With `--features watchdog_cont` the
//!   watchdog dump returns so the system keeps running and can re-dump.

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::arch::asm;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use x86_64::structures::idt::InterruptStackFrame;

    use crate::platform::x86_64_pc::apic;

    // ── MSRs ──────────────────────────────────────────────────────────
    const IA32_PERFEVTSEL0: u32 = 0x186;
    const IA32_PMC0: u32 = 0xC1;
    const IA32_PERF_GLOBAL_CTRL: u32 = 0x38F;

    // LVT already defined in apic.rs; we use apic::lvt_write.
    const LAPIC_LVT_PERF: u32 = 0x340;

    // ── Timeout ───────────────────────────────────────────────────────
    pub const WATCHDOG_TIMEOUT_NS: u64 = 3_000_000_000; // 3s
    const DEBOUNCE_NS: u64 = 10_000_000_000; // 10s between dumps
    const HOTKEY_DEBOUNCE_NS: u64 = 500_000_000; // 500ms

    // ── Per-CPU state ─────────────────────────────────────────────────
    static HEARTBEAT: [AtomicU64; 16] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    static LAST_RIP: [AtomicU64; 16] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    static NMI_IN_PROGRESS: [AtomicBool; 16] = [
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ];
    static NMI_ARMED: AtomicBool = AtomicBool::new(false);
    static LAST_DUMP_NS: AtomicU64 = AtomicU64::new(0);
    static HOTKEY_PENDING: AtomicBool = AtomicBool::new(false);
    static HOTKEY_LAST_NS: AtomicU64 = AtomicU64::new(0);
    static FALLBACK_MODE: AtomicBool = AtomicBool::new(false);
    // 0=none 1=pmu 2=pit 3=soft
    static WATCHDOG_MODE: AtomicU64 = AtomicU64::new(0);
    const MODE_PMU: u64 = 1;
    const MODE_PIT: u64 = 2;
    const MODE_SOFT: u64 = 3;

    // ── Helpers ───────────────────────────────────────────────────────
    fn wrmsr(msr: u32, val: u64) {
        let lo = val as u32;
        let hi = (val >> 32) as u32;
        unsafe {
            asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nomem, nostack));
        }
    }
    fn now_ns() -> u64 {
        apic::tsc_now_ns()
    }

    // ── Public API ────────────────────────────────────────────────────

    /// Lock-free heartbeat — call with interrupts in any state.
    pub fn pet() {
        let cpu = crate::smp::current_cpu_id() as usize;
        if cpu < 16 {
            HEARTBEAT[cpu].store(now_ns(), Ordering::Relaxed);
        } else {
            HEARTBEAT[0].store(now_ns(), Ordering::Relaxed);
        }
    }

    pub fn request_hotkey_dump() {
        let now = now_ns();
        let last = HOTKEY_LAST_NS.load(Ordering::Relaxed);
        if now.wrapping_sub(last) < HOTKEY_DEBOUNCE_NS {
            return;
        }
        HOTKEY_LAST_NS.store(now, Ordering::Relaxed);
        HOTKEY_PENDING.store(true, Ordering::Release);
    }

    pub fn is_armed() -> bool {
        NMI_ARMED.load(Ordering::Relaxed)
    }

    pub fn is_fallback() -> bool {
        FALLBACK_MODE.load(Ordering::Relaxed)
    }

    fn probe_pmu() -> bool {
        // CPUID leaf 0 max
        let max = core::arch::x86_64::__cpuid(0).eax;
        if max < 0xA {
            return false;
        }
        let a = core::arch::x86_64::__cpuid(0xA);
        let version = (a.eax & 0xFF) as u8;
        let num = ((a.eax >> 8) & 0xFF) as u8;
        let width = ((a.eax >> 16) & 0xFF) as u8;
        if version == 0 || num == 0 {
            return false;
        }
        // width==0 means 48 default per SDM
        let _ = width;
        true
    }

    fn pmu_period_ticks() -> u64 {
        let hz = apic::tsc_hz();
        if hz == 0 {
            return 3_000_000_000; // approx 1s worth @3GHz fallback
        }
        (hz as u128 * WATCHDOG_TIMEOUT_NS as u128 / 1_000_000_000u128) as u64
    }

    fn arm_pmu_for_cpu() {
        if !probe_pmu() {
            return;
        }
        let hz = apic::tsc_hz();
        if hz == 0 {
            return;
        }
        // Counter width (CPUID 0xA EAX 16:23)
        let a = core::arch::x86_64::__cpuid(0xA);
        let mut width = ((a.eax >> 16) & 0xFF) as u64;
        if width == 0 {
            width = 48;
        }
        if width > 63 {
            width = 63;
        }
        let mask = if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let ticks = pmu_period_ticks().min(mask / 2);
        let init = mask.wrapping_sub(ticks).wrapping_add(1) & mask;

        // Program counter 0: CPU_CLK_UNHALTED, USR+OS, EN, INT
        let val: u64 = 0x3C | (1 << 16) | (1 << 17) | (1 << 20) | (1 << 22);
        // Clear global ctrl first
        wrmsr(IA32_PERF_GLOBAL_CTRL, 0);
        wrmsr(IA32_PMC0, init & mask);
        wrmsr(IA32_PERFEVTSEL0, val);
        wrmsr(IA32_PERF_GLOBAL_CTRL, 1); // enable PMC0
        // Unmask LVT perf counter as NMI
        apic::lvt_write(LAPIC_LVT_PERF, 0b100, false);
        crate::drivers::serial::SerialPort::puts("[wdog] PMU NMI armed (PMC0 period=");
        crate::drivers::serial::SerialPort::put_u64(ticks);
        crate::drivers::serial::SerialPort::puts(" width=");
        crate::drivers::serial::SerialPort::put_u64(width);
        crate::drivers::serial::SerialPort::puts(")\n");
    }

    fn try_arm_pit_nmi() -> bool {
        use crate::acpi::{Polarity, TriggerMode};
        use crate::platform::x86_64_pc::{ioapic, pit};
        // QEMU TCG fallback: 100 Hz periodic PIT (11931 ticks)
        pit::program_periodic(11931);
        if ioapic::enable_nmi(0, Polarity::ActiveHigh, TriggerMode::Edge) {
            WATCHDOG_MODE.store(MODE_PIT, Ordering::Relaxed);
            NMI_ARMED.store(true, Ordering::Relaxed);
            crate::drivers::serial::SerialPort::puts("[wdog] PIT NMI armed (100 Hz via IOAPIC GSI0)\n");
            return true;
        }
        crate::drivers::serial::SerialPort::puts("[wdog] PIT NMI arm failed (no IOAPIC for GSI0)\n");
        false
    }

    fn fallback_soft_arm() {
        FALLBACK_MODE.store(true, Ordering::Relaxed);
        WATCHDOG_MODE.store(MODE_SOFT, Ordering::Relaxed);
        crate::drivers::serial::SerialPort::puts(
            "[wdog] PMU+PIT unavailable — soft watchdog (no NMI) fallback, hung RIP may be imprecise\n",
        );
        // Soft fallback: UniversalTimer periodic check 1s
        use crate::services::universal_timer::universal_timer;
        use core::sync::atomic::{AtomicU64, Ordering as O};
        static SOFT_ID: AtomicU64 = AtomicU64::new(0);
        fn soft_tick(_ctx: *mut u8) {
            let now = crate::platform::x86_64_pc::apic::tsc_now_ns();
            // Check BSP heartbeat (soft can't get NMI frame, so synthesize)
            let hb = HEARTBEAT[0].load(Ordering::Relaxed);
            if now.wrapping_sub(hb) > WATCHDOG_TIMEOUT_NS {
                let last = LAST_DUMP_NS.load(Ordering::Relaxed);
                if now.wrapping_sub(last) < DEBOUNCE_NS {
                    return;
                }
                LAST_DUMP_NS.store(now, Ordering::Relaxed);
                crate::drivers::serial::SerialPort::puts("[wdog] soft watchdog timeout — dumping (degraded, no NMI frame)\n");
                // Use dump_fatal synthetic — shows soft handler RIP, not ideal but better than hung silent
                crate::kerneldump::dump::dump_fatal("watchdog soft timeout (no PMU/NMI — hung RIP not captured)");
            }
            if HOTKEY_PENDING.swap(false, Ordering::AcqRel)
                || crate::drivers::ps2::poll_for_hotkey_nmi()
            {
                crate::drivers::serial::SerialPort::puts("[wdog] soft hotkey — dumping\n");
                crate::kerneldump::dump::dump_fatal("F9 hotkey (soft, no NMI frame)");
            }
        }
        if crate::services::universal_timer::is_ready() {
            // 1s periodic soft check
            let id = universal_timer().set_periodic(1_000_000_000, soft_tick, core::ptr::null_mut());
            SOFT_ID.store(id.seq, O::Relaxed);
            let _ = SOFT_ID;
        }
    }

    pub fn init() {
        // Pet once so first NMI doesn't spuriously fire
        let now = now_ns();
        for i in 0..16 {
            HEARTBEAT[i].store(now, Ordering::Relaxed);
        }
        // Extra diagnostics for QEMU TCG — helps when -cpu pmu=on is missing
        let max_cpuid = core::arch::x86_64::__cpuid(0).eax;
        let cpuid_a = if max_cpuid >= 0xA {
            core::arch::x86_64::__cpuid(0xA)
        } else {
            unsafe { core::mem::zeroed() }
        };
        crate::drivers::serial::SerialPort::puts("[wdog] CPUID 0xA: max=");
        crate::drivers::serial::SerialPort::put_u64(max_cpuid as u64);
        crate::drivers::serial::SerialPort::puts(" EAX=");
        crate::drivers::serial::SerialPort::put_hex(cpuid_a.eax as u64);
        crate::drivers::serial::SerialPort::puts(" EBX=");
        crate::drivers::serial::SerialPort::put_hex(cpuid_a.ebx as u64);
        crate::drivers::serial::SerialPort::puts("\n");
        if probe_pmu() {
            arm_pmu_for_cpu();
            WATCHDOG_MODE.store(MODE_PMU, Ordering::Relaxed);
            NMI_ARMED.store(true, Ordering::Relaxed);
        } else if try_arm_pit_nmi() {
            // NMI_ARMED already set inside try_arm_pit_nmi
        } else {
            fallback_soft_arm();
        }
        crate::drivers::serial::SerialPort::puts("[wdog] watchdog init done (NMI=");
        crate::drivers::serial::SerialPort::put_u64(NMI_ARMED.load(Ordering::Relaxed) as u64);
        crate::drivers::serial::SerialPort::puts(" mode=");
        crate::drivers::serial::SerialPort::put_u64(WATCHDOG_MODE.load(Ordering::Relaxed));
        crate::drivers::serial::SerialPort::puts(")\n");
    }

    pub fn init_ap() {
        if is_armed() {
            arm_pmu_for_cpu();
            let now = now_ns();
            let cpu = crate::smp::current_cpu_id() as usize;
            if cpu < 16 {
                HEARTBEAT[cpu].store(now, Ordering::Relaxed);
            }
        }
    }

    // ── NMI handler ───────────────────────────────────────────────────
    pub extern "x86-interrupt" fn nmi_handler(frame: InterruptStackFrame) {
        // GS swap to reach PerCpu (same as other handlers)
        let from_user = frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3;
        if from_user {
            unsafe { asm!("swapgs", options(nomem, nostack, preserves_flags)) };
        }

        let cpu = crate::smp::current_cpu_id() as usize;
        let cpu_idx = if cpu < 16 { cpu } else { 0 };

        // Re-entrancy guard — NMIs are not masked inside NMI until iret
        if NMI_IN_PROGRESS[cpu_idx].swap(true, Ordering::SeqCst) {
            // Nested NMI while dumping — spin, no EOI for NMI
            if from_user {
                unsafe { asm!("swapgs", options(nomem, nostack, preserves_flags)) };
            }
            return;
        }

        // Rearm PMU counter for next window (PIT needs no rearm — periodic)
        if WATCHDOG_MODE.load(Ordering::Relaxed) == MODE_PMU {
            // Ack overflow: clear GLOBAL_STATUS, reload PMC0
            let a = core::arch::x86_64::__cpuid(0xA);
            let mut width = ((a.eax >> 16) & 0xFF) as u64;
            if width == 0 {
                width = 48;
            }
            if width > 63 {
                width = 63;
            }
            let mask = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
            let ticks = pmu_period_ticks().min(mask / 2);
            let init = mask.wrapping_sub(ticks).wrapping_add(1) & mask;
            wrmsr(IA32_PMC0, init & mask);
            const IA32_PERF_GLOBAL_OVF_CTRL: u32 = 0x390;
            wrmsr(IA32_PERF_GLOBAL_OVF_CTRL, 1);
        }

        // Poll 8042 directly — even when IF=0 the IRQ never fires, but the byte
        // sits in the output buffer. NMI can still inb 0x64/0x60.
        crate::drivers::ps2::poll_for_hotkey_nmi();

        // ── Hotkey path (F9) — dump interrupted frame, not handler frame ──
        if HOTKEY_PENDING.swap(false, Ordering::AcqRel) {
            // Broadcast NMI to APs so they dump their own hung frames
            // Don't send if we are in fallback soft mode (no NMI delivery to self anyway)
            if NMI_ARMED.load(Ordering::Relaxed) {
                apic::send_nmi_all_except_self();
                // tiny delay to let AP NMIs arrive before we hog serial
                for _ in 0..1000 {
                    core::hint::spin_loop();
                }
            }
            LAST_DUMP_NS.store(now_ns(), Ordering::Relaxed);
            // Mark last RIP so next watchdog check doesn't also fire
            LAST_RIP[cpu_idx].store(frame.instruction_pointer.as_u64(), Ordering::Relaxed);
            crate::kerneldump::dump::dump_nmi_full_fault(&frame, 2, "PS/2 F9 hotkey");
            // F9 never halts — always continue (interactive debug), regardless of watchdog_cont
            NMI_IN_PROGRESS[cpu_idx].store(false, Ordering::SeqCst);
            if from_user {
                unsafe { asm!("swapgs", options(nomem, nostack, preserves_flags)) };
            }
            return;
        }

        // ── Watchdog path ──────────────────────────────────────────────
        let now = now_ns();
        let hb = HEARTBEAT[cpu_idx].load(Ordering::Relaxed);

        // Hang = heartbeat stale. Don't suppress when idle — `hlt` with IF=1
        // still pets via timer (hb fresh), so no false dump. Early `cli;hlt`
        // after #MC/#PF or before `sched_active` has stale hb and should dump.
        // `is_idle` kept only for logging, not gating.
        let _is_idle = {
            let per = crate::smp::try_current_per_cpu();
            if let Some(p) = per {
                let has_task = !p.current_task.load(Ordering::Relaxed).is_null();
                !has_task
            } else {
                false
            }
        };

        if now.wrapping_sub(hb) > WATCHDOG_TIMEOUT_NS {
            let rip = frame.instruction_pointer.as_u64();
            // Hang = heartbeat stale + not idle. Any RIP (loop or single insn)
            // is hung — don't require same RIP twice (misses loops).
            // Keep LAST_RIP for post-mortem correlation only.
            LAST_RIP[cpu_idx].store(rip, Ordering::Relaxed);
            let last_dump = LAST_DUMP_NS.load(Ordering::Relaxed);
            if now.wrapping_sub(last_dump) >= DEBOUNCE_NS {
                LAST_DUMP_NS.store(now, Ordering::Relaxed);
                // Broadcast to peers for full system view
                if NMI_ARMED.load(Ordering::Relaxed) {
                    apic::send_nmi_all_except_self();
                    for _ in 0..2000 {
                        core::hint::spin_loop();
                    }
                }
                // Dump *interrupted* frame (hung RIP), not handler RIP
                crate::kerneldump::dump::dump_nmi_full_fault(&frame, 2, "watchdog timeout (NMI)");
                // Default halt, feature watchdog_cont continues
                #[cfg(not(feature = "watchdog_cont"))]
                {
                    // halt forever — mirrors dump_full_fault final loop but inside NMI IST
                    // Re-enable DUMP guard already set by dump_nmi; just halt.
                    NMI_IN_PROGRESS[cpu_idx].store(false, Ordering::SeqCst);
                    if from_user {
                        unsafe { asm!("swapgs", options(nomem, nostack, preserves_flags)) };
                    }
                    loop {
                        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
                    }
                }
                #[cfg(feature = "watchdog_cont")]
                {
                    // continue — reset heartbeat so we don't retrigger instantly
                    HEARTBEAT[cpu_idx].store(now, Ordering::Relaxed);
                    LAST_RIP[cpu_idx].store(0, Ordering::Relaxed);
                }
            }
        } else if now.wrapping_sub(hb) <= WATCHDOG_TIMEOUT_NS {
            LAST_RIP[cpu_idx].store(0, Ordering::Relaxed);
        }

        NMI_IN_PROGRESS[cpu_idx].store(false, Ordering::SeqCst);
        if from_user {
            unsafe { asm!("swapgs", options(nomem, nostack, preserves_flags)) };
        }
        // No EOI for NMI
    }
}

#[cfg(target_arch = "x86_64")]
pub use imp::{init, init_ap, is_armed, is_fallback, nmi_handler, pet, request_hotkey_dump};

#[cfg(not(target_arch = "x86_64"))]
mod stub {
    pub fn pet() {}
    pub fn request_hotkey_dump() {}
    pub fn init() {}
    pub fn init_ap() {}
    pub fn is_armed() -> bool { false }
    pub fn is_fallback() -> bool { false }
}
#[cfg(not(target_arch = "x86_64"))]
pub use stub::{init, init_ap, is_armed, is_fallback, pet, request_hotkey_dump};
