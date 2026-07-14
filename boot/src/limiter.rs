#![allow(unsafe_op_in_unsafe_fn, dead_code, asm_sub_register)]

use core::arch::asm;

const IA32_PERF_CTL: u32 = 0x199;
const IA32_CLOCK_MODULATION: u32 = 0x19A;
const IA32_ENERGY_PERF_BIAS: u32 = 0x1B0;
const IA32_HWP_REQUEST: u32 = 0x774;

#[inline]
unsafe fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack));
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack));
    (low as u64) | ((high as u64) << 32)
}

#[inline]
unsafe fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    asm!(
        "push rbx",
        "cpuid",
        "mov {0}, rbx",
        "pop rbx",
        out(reg) ebx,
        inlateout("eax") leaf => eax,
        inlateout("ecx") subleaf => ecx,
        out("edx") edx,
    );
    (eax, ebx, ecx, edx)
}

fn is_intel() -> bool {
    let (_, ebx, edx, ecx) = unsafe { cpuid(0, 0) };
    let vendor: [u8; 12] = [
        ebx as u8, (ebx >> 8) as u8, (ebx >> 16) as u8, (ebx >> 24) as u8,
        edx as u8, (edx >> 8) as u8, (edx >> 16) as u8, (edx >> 24) as u8,
        ecx as u8, (ecx >> 8) as u8, (ecx >> 16) as u8, (ecx >> 24) as u8,
    ];
    &vendor == b"GenuineIntel"
}

pub unsafe fn enable_cpu_slow_mode() {
    if !is_intel() {
        return;
    }

    // IA32_CLOCK_MODULATION: On-Demand Clock Modulation.
    // Bit 0 = enable, bits 3:1 = duty cycle (001 = 12.5% / minimum).
    // Present on all Intel P4+; no CPUID check needed.
    unsafe { wrmsr(IA32_CLOCK_MODULATION, 0x3) };

    // IA32_ENERGY_PERF_BIAS: 0xF = max energy efficiency (slowest).
    // Present on Sandy Bridge+ (2011). Safe on any realistic Intel target.
    unsafe { wrmsr(IA32_ENERGY_PERF_BIAS, 0xF) };

    // IA32_PERF_CTL: Legacy P-state target (higher = lower frequency).
    // Only if EIST is supported (CPUID.01H:ECX[7]).
    let (_, _, ecx1, _) = unsafe { cpuid(1, 0) };
    if (ecx1 >> 7) & 1 == 1 {
        unsafe { wrmsr(IA32_PERF_CTL, 0xFF) };
    }

    // IA32_HWP_REQUEST: HWP min/max/desired + EPP.
    // Only if HWP supported (CPUID.06H:EAX[7]).
    if unsafe { cpuid(0, 0).0 >= 6 } {
        let (eax6, _, _, _) = unsafe { cpuid(6, 0) };
        if (eax6 >> 7) & 1 == 1 {
            let has_epp = (eax6 >> 10) & 1 == 1;
            let val = if has_epp {
                0xFF01_0101u64
            } else {
                0x0001_0101u64
            };
            unsafe { wrmsr(IA32_HWP_REQUEST, val) };
        }
    }
}
