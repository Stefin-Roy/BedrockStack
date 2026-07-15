use core::arch::asm;

const IA32_PERF_CTL: u32 = 0x199;
const IA32_CLOCK_MODULATION: u32 = 0x19A;
const IA32_ENERGY_PERF_BIAS: u32 = 0x1B0;
const IA32_HWP_REQUEST: u32 = 0x774;

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

fn is_intel() -> bool {
    let (_, ebx, edx, ecx) = unsafe { cpuid(0, 0) };
    let vendor: [u8; 12] = [
        ebx as u8, (ebx >> 8) as u8, (ebx >> 16) as u8, (ebx >> 24) as u8,
        edx as u8, (edx >> 8) as u8, (edx >> 16) as u8, (edx >> 24) as u8,
        ecx as u8, (ecx >> 8) as u8, (ecx >> 16) as u8, (ecx >> 24) as u8,
    ];
    &vendor == b"GenuineIntel"
}

fn is_hypervisor() -> bool {
    let (eax, _, _, _) = unsafe { cpuid(1, 0) };
    (eax >> 31) & 1 == 1
}

fn has_tm2() -> bool {
    let (_, _, ecx, _) = unsafe { cpuid(1, 0) };
    (ecx >> 4) & 1 == 1
}

fn has_eist() -> bool {
    let (_, _, ecx, _) = unsafe { cpuid(1, 0) };
    (ecx >> 7) & 1 == 1
}

fn has_energy_perf_bias() -> bool {
    if unsafe { cpuid(0, 0).0 } < 6 {
        return false;
    }
    let (_, _, ecx, _) = unsafe { cpuid(6, 0) };
    (ecx >> 3) & 1 == 1
}

fn has_hwp() -> bool {
    if unsafe { cpuid(0, 0).0 } < 6 {
        return false;
    }
    let (eax, _, _, _) = unsafe { cpuid(6, 0) };
    (eax >> 7) & 1 == 1
}

fn has_hwp_epp() -> bool {
    if unsafe { cpuid(0, 0).0 } < 6 {
        return false;
    }
    let (eax, _, _, _) = unsafe { cpuid(6, 0) };
    (eax >> 10) & 1 == 1
}

/// Configures CPU registers to force the lowest power and frequency state.
///
/// Only applies to bare-metal Intel systems.  Skips entirely when running
/// under a hypervisor (VMware, KVM, Hyper-V, …) because writing to
/// non-existent or trapped MSRs causes a #GP → triple fault.
pub unsafe fn enable_cpu_slow_mode() {
    if !is_intel() {
        return;
    }

    // Bail if running under a hypervisor — MSR accesses may #GP.
    if is_hypervisor() {
        return;
    }

    if has_tm2() {
        unsafe { wrmsr(IA32_CLOCK_MODULATION, 0x12) };
    }

    if has_energy_perf_bias() {
        unsafe { wrmsr(IA32_ENERGY_PERF_BIAS, 0xF) };
    }

    if has_eist() {
        unsafe { wrmsr(IA32_PERF_CTL, 0xFF00) };
    }

    if has_hwp() {
        let val = if has_hwp_epp() {
            0xFF01_0101u64
        } else {
            0x0001_0101u64
        };
        unsafe { wrmsr(IA32_HWP_REQUEST, val) };
    }
}
