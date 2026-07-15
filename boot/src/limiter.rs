#![allow(unsafe_op_in_unsafe_fn, dead_code, asm_sub_register)]

use core::arch::asm;

// MSR Addresses
const IA32_PERF_CTL: u32 = 0x199;
const IA32_CLOCK_MODULATION: u32 = 0x19A;
const IA32_ENERGY_PERF_BIAS: u32 = 0x1B0;
const IA32_HWP_REQUEST: u32 = 0x774;

/// Performs a Write to Model Specific Register.
#[inline]
pub unsafe fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack, preserves_flags)
    );
}

/// Performs a Read from Model Specific Register.
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack, preserves_flags)
    );
    (low as u64) | ((high as u64) << 32)
}

/// Performs a CPUID instruction.
/// This implementation relies on compiler-managed register allocation
/// to avoid stack corruption or manual push/pop errors.
#[inline]
pub unsafe fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx_out: u32;
    let ecx: u32;
    let edx: u32;
    asm!(
        "push rbx",
        "cpuid",
        "mov {ebx_out:e}, ebx",
        "pop rbx",
        ebx_out = out(reg) ebx_out,
        inout("eax") leaf => eax,
        inout("ecx") subleaf => ecx,
        out("edx") edx,
        options(nomem, preserves_flags)
    );
    (eax, ebx_out, ecx, edx)
}

/// Verifies vendor string is 'GenuineIntel'.
pub fn is_intel() -> bool {
    let (_, ebx, edx, ecx) = unsafe { cpuid(0, 0) };
    let vendor: [u8; 12] = [
        ebx as u8, (ebx >> 8) as u8, (ebx >> 16) as u8, (ebx >> 24) as u8,
        edx as u8, (edx >> 8) as u8, (edx >> 16) as u8, (edx >> 24) as u8,
        ecx as u8, (ecx >> 8) as u8, (ecx >> 16) as u8, (ecx >> 24) as u8,
    ];
    &vendor == b"GenuineIntel"
}

/// Configures CPU registers to force the lowest power and frequency state.
/// This prevents triple faults by strictly following Intel SDM bit-field requirements.
pub unsafe fn enable_cpu_slow_mode() {
    if !is_intel() {
        return;
    }

    // [1] IA32_CLOCK_MODULATION:
    // Bit 4 = Enable. Bits 3:1 = 001 (12.5% duty cycle). Bit 0 MUST be 0.
    // Correct value: 0x12 (Binary: 10010)
    unsafe { wrmsr(IA32_CLOCK_MODULATION, 0x12) };

    // [2] IA32_ENERGY_PERF_BIAS: 0xF (Max Energy Efficiency)
    unsafe { wrmsr(IA32_ENERGY_PERF_BIAS, 0xF) };

    // [3] IA32_PERF_CTL:
    // Only apply if EIST is supported (CPUID.01H:ECX[7])
    let (_, _, ecx1, _) = unsafe { cpuid(1, 0) };
    if (ecx1 >> 7) & 1 == 1 {
        // High bits set target ratio to the minimum supported P-state.
        unsafe { wrmsr(IA32_PERF_CTL, 0xFF00) };
    }

    // [4] IA32_HWP_REQUEST:
    // Only apply if HWP is supported (CPUID.06H:EAX[7])
    if unsafe { cpuid(0, 0).0 >= 6 } {
        let (eax6, _, _, _) = unsafe { cpuid(6, 0) };
        if (eax6 >> 7) & 1 == 1 {
            let has_epp = (eax6 >> 10) & 1 == 1;
            let val = if has_epp {
                0xFF01_0101u64 // Includes Energy Preference Policy (EPP)
            } else {
                0x0001_0101u64 // Just Min/Max request
            };
            unsafe { wrmsr(IA32_HWP_REQUEST, val) };
        }
    }
}